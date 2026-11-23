// SPDX-License-Identifier: MIT-0
//! `do27_chase`: DO27's chase bench on DO27's content through the full runtime frame
//! (`lookdev::chase`).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> do27_chase [--map switchbacks|bore_twelve] [--frames 180]
//!     [--warmup 60] [--size 2560x1440] [--held] [--start-m 0] [--csv <path>] [--png-dir <dir>]
//!     [--hold-after N]   (the paused-scene hold from loop frame N: gpu_frame p50 before and while held)
//!                  (`--size 3840x1600` is the owner's ultrawide, beside 1440p and 4K)
//!     [--plates]   (the six landscape UE plate cameras instead of the chase)
//!     [--sway]     (with --plates: sway the eye 2 cm per frame so the screen cache never hits)
//!     [--content <dir>]   (the DO27 content bundle, read-only; else REGIA_DO27_CONTENT)
//!     [--camera-path orbit|line|figure8]   (with --soak: the cut cameras follow this deterministic path)
//!     [--hitch-csv <path>]   (per-frame hitch CSV, `lookdev::soak_bundle`; <path> stem.<map>.csv with several maps)
//!     [--decals]   (a grid of decals around the view: the decal-receiver stamp and decal pass
//!                  run on every frame, e.g. an md5 check of that path with --plates --png-dir)
//!     [--sync]     (wait for each frame: no frames in flight) [--idle-ms N] (sleep after each)
//!     [--tier mid | hw,layers,vn,aniso=N]   (the opt-in terrain budget cuts; aniso=N the
//!                  terrain's anisotropic filtering level, 0 = off, default the tier's)
//!     [--vehicle-shadow all|local|off]
//!     [--scale 0.67] [--upscaler taau|tsr|fsr3] [--tsr-debug 1|2|3]   (temporal upscale of the TAA frame)
//!     [--tsr-texture-bias -0.1]   (TSR texture mip-bias offset; 1 = no bias)
//!     [--tsr-motion-sharpen 0.5]   (TSR extra output sharpening at full motion)
//!     [--fsr3-sharpness 0.8] [--fsr3-debug 1..6]   (FSR 3.1 RCAS strength, debug views)
//!     [--no-static-reuse]   (redraw cascades; no held-view screen reuse)
//!     [--vt [default|compact|texel=0.02,oversample=1,slots=32,near=24,bakes=48,…]]
//!                  (the terrain virtual texture, opt-in: its bake/miss/hit columns print too)
//!     [--soak long|minute] [--soak-seconds N]   (the hitch soak, `lookdev::soak`: every region,
//!                  camera cuts, time-of-day jumps, VFX/decal spawns; prints the specs/27 § 2.4 report)
//!     [--spawn-every-s S]   (the soak's VFX + decal spawn period, e.g. 0.25 for a spawn burst)
//!     [--memory-squeeze MB[:PERIOD_MS][:file]]   (a loaded host: from the first measured frame,
//!                  reclaim MB of this process's cgroup every PERIOD_MS (250), page cache only with
//!                  `file`, so cold code and data page out as on a box short of memory;
//!                  `regia_core::residency::MemorySqueeze`)
//!     [--no-keep-resident] [--keep-code-resident]   (no `regia_core::residency::ResidencyKeeper`,
//!                  or one that also sweeps the code; the runtime's `DefaultPlugins` run one over
//!                  the registered data, so the bench does too; REGIA_KEEP_RESIDENT=0|code)
//!     [--bare]     (the frame path of 2026-10-07: no FrameRenderer::prewarm, no scatter tile
//!                  budget, sky rebuilds inside the frame)
//!     [--slow-spans MS] (frames over MS print their span breakdown) [--census] (every
//!                  Device::create_* inside a frame; build with `--features wgpu-spans` to see
//!                  wgpu-core's own calls, including crates that hold a raw wgpu::Device)
//!     [--trace]    (log every frame-path create by label) [--timeline-dir <dir>]
//!     [--assert-no-late-pipelines]   (fail when any pipeline is created after the warm-up frames;
//!                  print each with its target format, sample count, thread, site and the
//!                  forward-set key axes that were new, ADR-7885)
//!                  (per map: <map>.csv timeline, <map>.png plot, <map>-report.txt with the spikes)
//! do27_chase --plot-csv <map>.csv   (re-plot a timeline into <map>.png; print its spike causes)
//! do27_chase --submit-probe N   (the machine's floor: N near-empty frames, `queue.submit` and
//!                  frame-time percentiles; a stall here is the driver or the box, not the engine)
//!     [--motion chase|pan|turn|dolly] [--deg 0.25 (dolly: m/frame)] [--pan-plate bore_twelve_close]
//!     [--seq-dir <dir>] [--every 10] [--converge 48] [--crops x,y,w,h;…] [--strip-frames 8]
//!                  (motion look: read every frame back, write frames, strips of crops and
//!                   motion.json — flicker, clarity, smear, pops; see `lookdev::motion`)
//!     [--off ssgi,ssao,ssr,fog,clouds,taa,lodfade,translucent-shadows]   (diagnosis: passes off, not the shell's frame)
//!     [--on radiance-cache,blue-noise,oit,sss,shadow-occlusion,lens-effects,rain|snow|vfx-layer,wet]   (opt-in
//!                  renderer features on, for their cost, and the weather probes: storm rain or
//!                  blizzard snow at the tier's budget, the wet surface response;
//!                  `lookdev::chase::ON_FEATURES`; `xtask gpu-bench --suite features` toggles each
//!                  against the plain frame)
//!     [--memory-json <path>]   (after the last frame: the `seeing.memory_report` fields of the
//!                  renderer's GPU memory and the heap allocations per measured frame, process-wide;
//!                  build with `--features alloc-count` to count them, else they are null)
//!     [--lens shutter=180,samples=2,focus=9,f=2.8,dof=gaussian|bokeh]   (the chase camera's
//!                  `PhysicalCamera`: motion blur and depth of field; any key may be left out)
//!     [--match]    (a live match's scene along the route, `lookdev::match_scene`: DO27's units,
//!                  parked vehicles, buildings and props at tester A's counts, 24 local lights
//!                  half shadowed, VFX emitters, a decal grid; prints drawn / instances /
//!                  casters / lights rows) [--match-seed N] [--match-parts N] [--match-lights N]
//!                  [--match-shadowed N] [--match-caster-m M] [--match-texture N]
//!                  [--match-flat]   (the match's materials without textures: their texture cost)
//!     [--off prepass]   (no depth prepass: the forward pass pays the overdraw)
//! do27_chase --list-gpus   (the adapters REGIA_GPU_INDEX/_NAME/_PCI/_UUID select from)
//!     [--window]   (present into a winit window titled regia.do27_chase; desktop only;
//!                  headless boxes need xvfb-run -a; not with --seq-dir)
//!     [--summary <path>]  (spec 28 summary.json for this process: schema 1, exit.clean)
//!     [--scenario <name>] (host.scenario in that summary; default chase)
//!     [--seconds N]  (measured frames = round(N / (1/60)), instead of --frames)
//!     [--crash-test]  (write summary.json with exit.panic true, then panic; no map load)
//! do27_chase --map switchbacks --prepare-cache <dir>   (install prepare of the maps,
//!                  `regia_render::prepare::run` with each map's chase warm as its job, ADR-8453:
//!                  fills <dir> the way a game's first-launch prepare does, then exits; a later
//!                  run with REGIA_PIPELINE_CACHE_DIR=<dir> warms from that cache)
//! ```

use std::io::Write as _;
use std::process::ExitCode;

use lookdev::chase::{ChaseOptions, rows};
use lookdev::look_r2::{R2Planets, R2Shot};
use lookdev::{LookdevError, Planet};

// `--features alloc-count`: count every heap allocation (`--memory-json`'s allocations per
// frame). Off in every other build: the header it adds to each block moves RSS numbers.
#[cfg(feature = "alloc-count")]
#[global_allocator]
static ALLOC: regia_core::memory::TaggingAllocator = regia_core::memory::TaggingAllocator::system();

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let slow = value(&args, "--slow-spans").and_then(|v| v.parse::<f64>().ok());
    let census = args.iter().any(|a| a == "--census");
    if std::env::var("CHASE_WARM_SPANS").is_ok_and(|v| v == "1")
        && let Some(spans) = lookdev::spans::install_systems()
    {
        let _ = lookdev::spans::WARM_SPANS.set(spans);
    }
    if (slow.is_some() || census) && !lookdev::spans::install(slow, census) {
        eprintln!("do27_chase: a tracing subscriber was already installed; span tools are off");
    }
    match main_inner(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("do27_chase: {err}");
            if let Some(path) = value(&args, "--summary")
                && let Err(write_err) = lookdev::session_summary::write_failure(
                    std::path::Path::new(path),
                    scenario_of(&args),
                    &err.to_string(),
                )
            {
                eprintln!("do27_chase: summary: {write_err}");
            }
            ExitCode::from(1)
        }
    }
}

fn scenario_of(args: &[String]) -> &str {
    value(args, "--scenario").unwrap_or("chase")
}

/// `--lens shutter=180,samples=2,focus=9,f=2.8,dof=gaussian|bokeh,sensor=0.024,max_coc=48`.
fn parse_lens(spec: &str) -> Result<regia_render::PhysicalCamera, LookdevError> {
    let mut lens = regia_render::PhysicalCamera::default();
    for item in spec.split(',').filter(|s| !s.is_empty()) {
        let (key, v) = item
            .split_once('=')
            .ok_or_else(|| LookdevError::Config(format!("--lens: `{item}` is not key=value")))?;
        let num = || {
            v.parse::<f32>()
                .map_err(|_| LookdevError::Config(format!("--lens: `{item}` is not a number")))
        };
        match key {
            "shutter" => lens.shutter_angle = num()? / 360.0,
            "samples" => lens.motion_blur_samples = num()? as u32,
            "focus" => lens.focal_distance = num()?,
            "f" => lens.aperture_f_stops = num()?,
            "sensor" => lens.sensor_height = num()?,
            "max_coc" => lens.max_circle_of_confusion_diameter = num()?,
            "dof" => {
                lens.depth_of_field = match v {
                    "gaussian" => Some(regia_render::DepthOfFieldMode::Gaussian),
                    "bokeh" => Some(regia_render::DepthOfFieldMode::Bokeh),
                    "off" => None,
                    _ => return Err(LookdevError::Config(format!("--lens: unknown dof `{v}`"))),
                }
            }
            _ => return Err(LookdevError::Config(format!("--lens: unknown key `{key}`"))),
        }
    }
    Ok(lens)
}

/// SSIM (`regia_render::compare::ssim`, luma, 11×11 Gaussian) of every PNG in both
/// directories, the largest channel difference and the share of pixels more than 8 levels
/// off; writes `diff_<name>.png` (|Δ| × 8) into `after`.
fn compare_dirs(before: &std::path::Path, after: &std::path::Path) -> Result<(), LookdevError> {
    use lookdev::default_look::{read_png, write_png};
    let io = |path: &std::path::Path, source: std::io::Error| LookdevError::Io {
        path: path.display().to_string(),
        source,
    };
    let mut names: Vec<_> = std::fs::read_dir(before)
        .map_err(|e| io(before, e))?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".png") && !n.starts_with("diff_"))
        .collect();
    names.sort();
    for name in names {
        let (Ok(a), Ok(b)) = (read_png(&before.join(&name)), read_png(&after.join(&name))) else {
            continue;
        };
        if (a.0, a.1) != (b.0, b.1) {
            println!("{name}: sizes differ");
            continue;
        }
        let rgb = |px: &[u8]| -> Vec<[f32; 3]> {
            px.as_chunks::<4>()
                .0
                .iter()
                .map(|c| [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0])
                .collect()
        };
        let ssim = regia_render::compare::ssim(&rgb(&a.2), &rgb(&b.2), a.0, a.1);
        let mut max = 0u8;
        let mut over = 0usize;
        let mut diff = Vec::with_capacity(a.2.len());
        for (pa, pb) in a.2.as_chunks::<4>().0.iter().zip(b.2.as_chunks::<4>().0) {
            let d = (0..3).map(|k| pa[k].abs_diff(pb[k])).max().unwrap_or(0);
            max = max.max(d);
            over += usize::from(d > 8);
            for k in 0..3 {
                diff.push(pa[k].abs_diff(pb[k]).saturating_mul(8));
            }
            diff.push(255);
        }
        let n = (a.0 * a.1) as f32;
        println!(
            "{name}: ssim {ssim:.5}, max |d| {max}, px > 8 levels {:.3} %",
            over as f32 / n * 100.0
        );
        write_png(&after.join(format!("diff_{name}")), a.0, a.1, &diff)?;
    }
    Ok(())
}

/// `default`, `compact`, or `key=value` pairs over `VirtualTextureSettings::default()` (`texel=0.02,
/// oversample=1,page=126,slots=32,near=24,min_ny=0.25,bakes=48,rebakes=4,stale=1`); `full` (the
/// default, `VtCache::Surface`), `relief` or `lighting` choose what is cached; `trilinear` reads both LODs.
fn vt_settings(spec: &str) -> Result<regia_terrain::VirtualTextureSettings, LookdevError> {
    let mut s = regia_terrain::VirtualTextureSettings::default();
    for part in spec.split(',') {
        let num = |v: &str| {
            v.parse::<f32>()
                .map_err(|_| LookdevError::Config(format!("--vt {part}: not a number")))
        };
        match part.split_once('=') {
            None if part == "default" => {}
            None if part == "compact" => s = regia_terrain::VirtualTextureSettings::compact(),
            Some(("texel", v)) => s.lod0_texel_m = num(v)?,
            Some(("oversample", v)) => s.oversample = num(v)?,
            Some(("slots", v)) => s.atlas_slots = num(v)? as u32,
            Some(("page", v)) => s.page_texels = num(v)? as u32,
            Some(("near", v)) => s.near_m = num(v)?,
            Some(("min_ny", v)) => s.min_normal_y = num(v)?,
            Some(("bakes", v)) => s.bakes_per_frame = num(v)? as u32,
            Some(("rebakes", v)) => s.rebakes_per_frame = num(v)? as u32,
            Some(("stale", v)) => s.stale_threshold = num(v)?,
            None if part == "full" || part == "surface" => s.cache = regia_terrain::VtCache::Surface,
            None if part == "lighting" => s.cache = regia_terrain::VtCache::Lighting,
            None if part == "relief" => s.cache = regia_terrain::VtCache::Relief,
            None if part == "trilinear" => s.stochastic_lod = false,
            _ => return Err(LookdevError::Config(format!("--vt: unknown setting {part}"))),
        }
    }
    Ok(s)
}

/// `--match` and its `--match-*` knobs over [`lookdev::match_scene::MatchPlan::TESTER_A`].
fn match_options(args: &[String]) -> Result<Option<lookdev::match_scene::MatchOptions>, LookdevError> {
    if !args.iter().any(|a| a == "--match") {
        return Ok(None);
    }
    let mut options = lookdev::match_scene::MatchOptions::default();
    let num = |flag: &str| -> Result<Option<u64>, LookdevError> {
        value(args, flag)
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| LookdevError::Config(format!("{flag} {v}: not a whole number")))
            })
            .transpose()
    };
    let small = |flag: &str, v: u64| {
        u32::try_from(v).map_err(|_| LookdevError::Config(format!("{flag} {v}: too large")))
    };
    if let Some(v) = num("--match-seed")? {
        options.plan.seed = v;
    }
    if let Some(v) = num("--match-parts")? {
        options.plan.part_instances = small("--match-parts", v)?;
    }
    if let Some(v) = num("--match-lights")? {
        options.plan.lights = small("--match-lights", v)?;
        options.plan.shadowed_lights = options.plan.shadowed_lights.min(options.plan.lights);
    }
    if let Some(v) = num("--match-shadowed")? {
        options.plan.shadowed_lights = small("--match-shadowed", v)?.min(options.plan.lights);
    }
    if let Some(v) = num("--match-texture")? {
        options.max_texture = small("--match-texture", v)?.max(1);
    }
    if let Some(v) = value(args, "--match-caster-m") {
        options.plan.caster_range_m = v
            .parse()
            .map_err(|_| LookdevError::Config(format!("--match-caster-m {v}: not a number")))?;
    }
    options.flat = args.iter().any(|a| a == "--match-flat");
    options.proxy = args.iter().any(|a| a == "--match-proxy");
    Ok(Some(options))
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn main_inner(args: &[String]) -> Result<(), LookdevError> {
    if args.iter().any(|a| a == "--crash-test") {
        let scenario = scenario_of(args);
        if let Some(path) = value(args, "--summary") {
            lookdev::session_summary::write_panic(std::path::Path::new(path), scenario)?;
        }
        panic!("do27_chase --crash-test");
    }
    if let Some(i) = args.iter().position(|a| a == "--compare") {
        let (Some(a), Some(b)) = (args.get(i + 1), args.get(i + 2)) else {
            return Err(LookdevError::Config("--compare <before_dir> <after_dir>".into()));
        };
        return compare_dirs(std::path::Path::new(a), std::path::Path::new(b));
    }
    if args.iter().any(|a| a == "--list-gpus") {
        // What `REGIA_GPU_INDEX` / `_NAME` / `_PCI` / `_UUID` can select (Vulkan order).
        for adapter in regia_render::list_adapters() {
            println!("{adapter}");
        }
        return Ok(());
    }
    if let Some(csv) = value(args, "--plot-csv") {
        // Re-plot a timeline: `--plot-csv <map>.csv` writes `<map>.png` beside it.
        let csv = std::path::Path::new(csv);
        let samples = lookdev::soak::samples_from_csv(csv)?;
        let mut walls: Vec<f32> = samples.iter().map(|s| s.wall_ms).collect();
        walls.sort_by(f32::total_cmp);
        let p50 = regia_render::hitch::percentile_sorted(&walls, 0.5);
        let png = csv.with_extension("png");
        lookdev::soak_plot::write_timeline(&png, &samples, p50, 2400)?;
        println!("wrote {}", png.display());
        println!(
            "p50 {p50:.3} ms; {}",
            lookdev::soak::spike_causes(&samples, p50).line(samples.len())
        );
        return Ok(());
    }
    if let Some(frames) = value(args, "--submit-probe").and_then(|v| v.parse::<u32>().ok()) {
        return submit_probe(&lookdev::chase::runtime_device()?, frames);
    }
    // `--content <dir>` names the bundle on the command line; it wins over REGIA_DO27_CONTENT.
    let content = value(args, "--content")
        .map(std::path::PathBuf::from)
        .or_else(lookdev::do27::content_dir)
        .ok_or_else(|| {
            LookdevError::Config("pass --content <dir> or set REGIA_DO27_CONTENT=<do27-content dir>".into())
        })?;
    let bundle = lookdev::soak_bundle::Bundle::open(&content)?;
    let hitch_csv = value(args, "--hitch-csv").map(std::path::PathBuf::from);
    if let Some(path) = hitch_csv.as_ref() {
        bundle.ensure_outside(path)?;
    }
    let mut options = ChaseOptions::default();
    if let Some(name) = value(args, "--camera-path") {
        options.camera_path = Some(lookdev::soak_bundle::CameraPath::parse(name)?);
    }
    if let Some(v) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        options.frames = v;
    }
    if let Some(seconds) = value(args, "--seconds") {
        let seconds = seconds
            .parse::<f64>()
            .map_err(|_| LookdevError::Config(format!("--seconds {seconds} is not a number")))?;
        options.frames = lookdev::session_summary::frames_for_seconds(seconds)
            .ok_or_else(|| LookdevError::Config(format!("--seconds {seconds} is not a positive duration")))?;
    }
    if let Some(v) = value(args, "--warmup").and_then(|v| v.parse().ok()) {
        options.warmup = v;
    }
    if let Some(v) = value(args, "--start-m").and_then(|v| v.parse().ok()) {
        options.start_m = v;
    }
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    options.held = args.iter().any(|a| a == "--held");
    // `--hold-after N`: the paused-scene hold from loop frame N (warm-up included, ADR-7890).
    if let Some(n) = value(args, "--hold-after") {
        options.hold_after = Some(
            n.parse()
                .map_err(|_| LookdevError::Config(format!("--hold-after {n} is not a frame number")))?,
        );
    }
    let png_dir = value(args, "--png-dir").map(std::path::PathBuf::from);
    let plates = args.iter().any(|a| a == "--plates");
    options.sway = args.iter().any(|a| a == "--sway");
    // `--vehicle-shadow local` (a short caster: its own cascade and the next) or `off`.
    match value(args, "--vehicle-shadow") {
        Some("local") => {
            options.vehicle_shadow = regia_render::ShadowCast::Local {
                reach: 12.0,
                max_cascades: 2,
                shadow_scale: 1.0,
                shadow_lift: 0.0,
                anchor: None,
                lod_switch_keeps_pages: false,
            };
        }
        Some("off") => options.vehicle_shadow = regia_render::ShadowCast::Off,
        _ => {}
    }
    // `--tier mid` (all three cuts) or a comma list of `hw`, `layers`, `vn`.
    if let Some(tier) = value(args, "--tier") {
        let mut budget = regia_render::TerrainBudget::OFF;
        for part in tier.split(',') {
            match part {
                "mid" => budget = regia_render::TerrainBudget::MID_RANGE,
                "hw" => budget.hw_filtered_normals = true,
                "layers" => budget.far_layers_m = regia_render::TerrainBudget::MID_RANGE.far_layers_m,
                "vn" => budget.vertex_normal_m = regia_render::TerrainBudget::MID_RANGE.vertex_normal_m,
                _ => {
                    if let Some(level) = part.strip_prefix("aniso=").and_then(|v| v.parse().ok()) {
                        options.terrain_anisotropy = Some(level);
                    }
                }
            }
        }
        options.budget = budget;
    }
    // `--scale 0.67` renders the TAA frame at 67 % and upscales; `--upscaler tsr` picks
    // the TSR-grade upscaler instead of the TAAU default (`specs/27` § 2.3).
    if let Some(v) = value(args, "--scale").and_then(|v| v.parse().ok()) {
        options.render_scale = Some(v);
    }
    match value(args, "--upscaler") {
        Some("tsr") => options.upscaler = regia_render::TemporalUpscaler::Tsr,
        Some("fsr3") => options.upscaler = regia_render::TemporalUpscaler::Fsr3,
        _ => {}
    }
    // `--fsr3-sharpness S` (RCAS, 0–1; 0 = off) and `--fsr3-debug N` (1 accumulation, 2 lock,
    // 3 disocclusion, 4 shading change, 5 luma instability, 6 reactive).
    if let Some(sharpness) = value(args, "--fsr3-sharpness").and_then(|v| v.parse::<f32>().ok()) {
        let mut settings = options.fsr3.unwrap_or_default();
        settings.sharpness = sharpness;
        options.fsr3 = Some(settings);
    }
    if let Some(view) = value(args, "--fsr3-debug").and_then(|v| v.parse::<u32>().ok()) {
        let mut settings = options.fsr3.unwrap_or_default();
        settings.debug_view = view;
        options.fsr3 = Some(settings);
    }
    // `--tsr-debug N`: the TSR debug view (1 weight, 2 lock, 3 disocclusion).
    if let Some(view) = value(args, "--tsr-debug").and_then(|v| v.parse().ok()) {
        let scale = options.render_scale.unwrap_or(1.0);
        options.tsr = Some(regia_render::TsrSettings {
            debug_view: view,
            ..regia_render::TsrSettings::for_scale(scale)
        });
    }
    // `--tsr-ridge X`: the TSR luma-ridge lock threshold (large = no luma locks).
    if let Some(ridge) = value(args, "--tsr-ridge").and_then(|v| v.parse::<f32>().ok()) {
        let scale = options.render_scale.unwrap_or(1.0);
        let mut settings = options
            .tsr
            .unwrap_or_else(|| regia_render::TsrSettings::for_scale(scale));
        settings.ridge_threshold = ridge;
        options.tsr = Some(settings);
    }
    // `--tsr-texture-bias X`: offset added to log2(render / output) for the texture mip
    // bias while TSR upscales (≥ 0.6 at scale 0.67 turns the bias off).
    if let Some(offset) = value(args, "--tsr-texture-bias").and_then(|v| v.parse::<f32>().ok()) {
        let scale = options.render_scale.unwrap_or(1.0);
        let mut settings = options
            .tsr
            .unwrap_or_else(|| regia_render::TsrSettings::for_scale(scale));
        settings.texture_bias_offset = offset;
        options.tsr = Some(settings);
    }
    // `--tsr-motion-sharpen X`: TSR's extra output sharpening at full motion.
    if let Some(amount) = value(args, "--tsr-motion-sharpen").and_then(|v| v.parse::<f32>().ok()) {
        let scale = options.render_scale.unwrap_or(1.0);
        let mut settings = options
            .tsr
            .unwrap_or_else(|| regia_render::TsrSettings::for_scale(scale));
        settings.motion_sharpness = amount;
        options.tsr = Some(settings);
    }
    options.no_static_reuse = args.iter().any(|a| a == "--no-static-reuse");
    // --vt [default|compact|k=v,...]: the terrain virtual texture (opt-in, ADR-117).
    if let Some(i) = args.iter().position(|a| a == "--vt") {
        let spec = args
            .get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .map_or("default", String::as_str);
        options.vt = Some(vt_settings(spec)?);
    }
    let deg = value(args, "--deg").and_then(|v| v.parse::<f32>().ok());
    match value(args, "--motion") {
        Some("pan") => {
            let plate = value(args, "--pan-plate").unwrap_or("bore_twelve_close");
            let plate = R2Shot::PARITY
                .into_iter()
                .find(|s| s.name() == plate)
                .ok_or_else(|| LookdevError::Config(format!("no plate {plate}")))?;
            options.motion = lookdev::motion::Motion::Pan {
                plate,
                deg_per_frame: deg.unwrap_or(0.25),
            };
        }
        Some("turn") => {
            options.motion = lookdev::motion::Motion::Turn {
                deg_per_frame: deg.unwrap_or(2.0),
            };
        }
        Some("dolly") => {
            options.motion = lookdev::motion::Motion::Dolly {
                m_per_frame: deg.unwrap_or(0.25),
            };
        }
        Some("chase") | None => {}
        Some(other) => {
            return Err(LookdevError::Config(format!(
                "--motion {other}: chase|pan|turn|dolly"
            )));
        }
    }
    if let Some(dir) = value(args, "--seq-dir") {
        options.seq = Some(lookdev::motion::SeqOptions {
            dir: dir.into(),
            every: value(args, "--every").and_then(|v| v.parse().ok()).unwrap_or(10),
            converge: value(args, "--converge")
                .and_then(|v| v.parse().ok())
                .unwrap_or(48),
            crops: value(args, "--crops")
                .map(lookdev::motion::parse_crops)
                .unwrap_or_default(),
            strip_frames: value(args, "--strip-frames")
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
        });
    }
    let windowed = args.iter().any(|a| a == "--window");
    if windowed && options.seq.is_some() {
        return Err(LookdevError::Config(
            "--window reads the swapchain; it cannot be combined with --seq-dir".into(),
        ));
    }
    #[cfg(target_os = "android")]
    if windowed {
        return Err(LookdevError::Config(
            "do27_chase --window is desktop-only; the Android shell presents through regia::run".into(),
        ));
    }
    if let Some(off) = value(args, "--off") {
        options.off = off.split(',').map(str::to_owned).collect();
    }
    if let Some(on) = value(args, "--on") {
        options.on = on.split(',').map(str::to_owned).collect();
    }
    let memory_json = value(args, "--memory-json").map(std::path::PathBuf::from);
    if let Some(spec) = value(args, "--lens") {
        options.lens = Some(parse_lens(spec)?);
    }
    options.baked_gi = value(args, "--baked-gi").map(std::path::PathBuf::from);
    options.match_scene = match_options(args)?;
    options.sync = args.iter().any(|a| a == "--sync");
    options.decals = args.iter().any(|a| a == "--decals");
    options.soak = match value(args, "--soak") {
        Some("long") => Some(lookdev::soak::SoakOptions::LONG),
        Some("minute") => Some(lookdev::soak::SoakOptions::MINUTE),
        Some(other) => return Err(LookdevError::Config(format!("--soak {other}: long|minute"))),
        None => None,
    };
    if memory_json.is_some() && (options.soak.is_some() || plates) {
        // The soak's frame count is its script's, and the plates would share one file name.
        return Err(LookdevError::Config(
            "--memory-json measures one chase: not with --soak or --plates".into(),
        ));
    }
    if options.soak.is_some() && value(args, "--warmup").is_none() {
        // specs/27 § 2.4: measured after a 2 s warm-up (60 Hz drive steps).
        options.warmup = 120;
    }
    if let (Some(soak), Some(v)) = (
        options.soak.as_mut(),
        value(args, "--soak-seconds").and_then(|v| v.parse().ok()),
    ) {
        soak.seconds = v;
    }
    if let (Some(soak), Some(v)) = (
        options.soak.as_mut(),
        value(args, "--spawn-every-s").and_then(|v| v.parse().ok()),
    ) {
        soak.spawn_every_s = v;
    }
    let timeline_dir = value(args, "--timeline-dir").map(std::path::PathBuf::from);
    let keep_env = std::env::var("REGIA_KEEP_RESIDENT").ok();
    let keep_code = args.iter().any(|a| a == "--keep-code-resident") || keep_env.as_deref() == Some("code");
    let keeper = (!args.iter().any(|a| a == "--no-keep-resident") && keep_env.as_deref() != Some("0"))
        .then(|| {
            regia_core::residency::ResidencyKeeper::start(regia_core::residency::ResidencySettings {
                code: keep_code,
                ..Default::default()
            })
        })
        .flatten();
    println!(
        "residency keeper: {}",
        match (keeper.is_some(), keep_code) {
            (false, _) => "off",
            (true, false) => "on (registered data: terrain heights)",
            (true, true) => "on (registered data and code)",
        }
    );
    options.memory_squeeze =
        match value(args, "--memory-squeeze") {
            Some(v) => Some(regia_core::residency::SqueezeSettings::parse(v).ok_or_else(|| {
                LookdevError::Config(format!("--memory-squeeze {v}: MB[:PERIOD_MS][:file]"))
            })?),
            None => None,
        };
    if args.iter().any(|a| a == "--bare") {
        options.prewarm = false;
        options.async_sky = false;
    }
    let assert_no_late = args.iter().any(|a| a == "--assert-no-late-pipelines");
    options.trace = args.iter().any(|a| a == "--trace") || options.soak.is_some() || assert_no_late;
    let mut failed_gate = false;
    let mut late_pipelines = 0u64;
    if let Some(v) = value(args, "--idle-ms").and_then(|v| v.parse().ok()) {
        options.idle_ms = v;
    }
    let pan_map = match options.motion {
        lookdev::motion::Motion::Pan { plate, .. } => Some(plate.planet()),
        _ => None,
    };
    let maps: Vec<Planet> = match value(args, "--map") {
        _ if pan_map.is_some() => pan_map.into_iter().collect(),
        Some("switchbacks") => vec![Planet::Mars],
        Some("bore_twelve") => vec![Planet::Europa],
        _ => vec![Planet::Europa, Planet::Mars],
    };
    let maps_len = maps.len();
    // `--prepare-cache <dir>` (ADR-8453): before this process opens its own device, which would
    // hold the cache.
    if let Some(dir) = value(args, "--prepare-cache") {
        return prepare_maps(std::path::Path::new(dir), &content, &maps, &options);
    }
    // Read before the device compiles anything, so an empty cache dir is a miss.
    let cache_label = lookdev::session_summary::cache_label_now().to_owned();
    let device = lookdev::chase::runtime_device()?;
    let info = device.adapter.get_info();
    println!("adapter: {}", info.name);
    let gpu_name = info.name.clone();
    let backend = format!("{:?}", info.backend);
    #[cfg(not(target_os = "android"))]
    let mut window_host = if windowed {
        Some(lookdev::window_host::WindowHost::open(
            options.width,
            options.height,
        )?)
    } else {
        None
    };
    let summary_path = value(args, "--summary").map(std::path::PathBuf::from);
    let scenario = scenario_of(args).to_owned();
    let mut planets = R2Planets::default();
    let mut csv = String::from(
        "map,frame,wall,cpu,encode,submit,scatter,gpu_frame,shadows,clusters,prepass,cull,ssao,forward,ssgi,ssr,atmosphere,clouds,fog,taau,post,rocks,terrain_forward,vt_bake,vt_reduce,vt_baked,vt_rebaked,vt_missing,vt_oldest_miss_frames,vt_hit_rate,sssr_trace,sssr_denoise,sssr_quads,sssr_tiles\n",
    );
    // The chase on each map, or the six landscape plate cameras of each map (`--plates`).
    let mut jobs: Vec<(Planet, Option<R2Shot>)> = Vec::new();
    for planet in maps {
        if plates {
            for shot in R2Shot::PARITY {
                let landscape = matches!(
                    shot,
                    R2Shot::BoreTwelveWide
                        | R2Shot::BoreTwelveMid
                        | R2Shot::BoreTwelveClose
                        | R2Shot::SwitchbacksWide
                        | R2Shot::SwitchbacksMid
                        | R2Shot::SwitchbacksClose
                );
                if landscape && shot.planet() == planet {
                    jobs.push((planet, Some(shot)));
                }
            }
        } else {
            jobs.push((planet, None));
        }
    }
    let mut walls = Vec::new();
    let mut maps_run = Vec::new();
    let mut prewarm_ms = 0.0f32;
    if let Some(path) = summary_path.as_deref() {
        lookdev::session_summary::write_partial(
            path,
            &lookdev::session_summary::RunRecord {
                scenario: scenario.clone(),
                window: windowed,
                maps: Vec::new(),
                width: options.width,
                height: options.height,
                wall_ms: Vec::new(),
                prewarm_ms: 0.0,
                cache: cache_label.clone(),
                gpu: gpu_name.clone(),
                backend: backend.clone(),
                content: content.display().to_string(),
                clean: false,
                panic: false,
                note: String::new(),
            },
        )?;
    }
    for (planet, plate) in jobs {
        let loaded = std::time::Instant::now();
        options.plate = plate;
        options.held = options.held || plate.is_some();
        let name = plate.map_or(lookdev::do27::map_name(planet), R2Shot::name);
        options.png = png_dir.as_ref().map(|d| d.join(format!("{name}.png")));
        // `--memory-json`: the renderer's GPU memory after the last measured frame (while its
        // targets are alive) and the heap allocations of the measured frames.
        let mut alloc_window = lookdev::feature_cost::AllocWindow::new(options.warmup, options.frames);
        let mut memory: Option<regia_render::MemoryReport> = None;
        let mut after_frame = |i: u32, _: &regia_render::FrameRenderer| {
            let total = lookdev::feature_cost::total_allocations().unwrap_or(0);
            if alloc_window.after_frame(i, total) && memory_json.is_some() {
                memory = Some(regia_render::memory_report(&device));
            }
        };
        #[cfg(not(target_os = "android"))]
        let mut result = chase_job(
            &device,
            &mut planets,
            &content,
            planet,
            &options,
            window_host
                .as_mut()
                .map(|host| host as &mut dyn lookdev::chase::FramePresent),
            &mut after_frame,
        )?;
        #[cfg(target_os = "android")]
        let mut result = lookdev::chase::run_with(
            &device,
            &mut planets,
            &content,
            planet,
            &options,
            &mut after_frame,
        )?;
        if let Some(path) = memory_json.as_ref() {
            let out = if maps_len > 1 {
                path.with_extension(format!("{}.json", result.map))
            } else {
                path.clone()
            };
            match memory.as_ref() {
                Some(report) => {
                    let tracked = lookdev::feature_cost::total_allocations().is_some();
                    let doc = lookdev::feature_cost::memory_json(
                        report,
                        tracked.then_some(&alloc_window),
                        &options.on,
                        20,
                    );
                    lookdev::feature_cost::write_json(&out, &doc)?;
                    println!("wrote {}", out.display());
                }
                None => eprintln!(
                    "  warning: --memory-json: the run ended before its last measured frame; nothing written"
                ),
            }
        }
        if plate.is_some() {
            result.map = name;
        }
        walls.extend(result.samples.iter().map(|sample| sample.wall_ms));
        maps_run.push(result.map.to_owned());
        prewarm_ms += result.prewarm_ms;
        println!(
            "\n{} {}x{} {} frames{} (route {:.0} m, setup {:.1} s): submits/frame {}, terrain cascade draws {} (+{} page-region), \
             device calls after warm-up: {} pipelines, {} bind groups, {} blocking polls, {} map_async",
            result.map,
            options.width,
            options.height,
            result.samples.len(),
            if options.held { " held" } else { "" },
            result.route_m,
            loaded.elapsed().as_secs_f32(),
            result.submits,
            result.cascade_draws,
            result.region_draws,
            result.counters.pipelines,
            result.counters.bind_groups,
            result.counters.blocking_polls,
            result.counters.map_asyncs,
        );
        // Every creation of the process so far, warm-up included (cold: driver compiles; after a
        // prepare: driver-cache hits; ADR-8453).
        println!(
            "  pipelines created this process: {}",
            regia_render::pipeline_cache::pipelines_created()
        );
        if assert_no_late {
            late_pipelines += late_pipeline_report(result.map, result.counters.pipelines, &result.events);
        }
        let rocks: f32 =
            result.samples.iter().map(|s| s.rocks as f32).sum::<f32>() / result.samples.len().max(1) as f32;
        println!("  rocks drawn (mean) {rocks:.0}");
        if let Some(report) = result.match_report.as_ref() {
            println!("  {}", report.line());
        }
        println!(
            "  gi: {} (baked volume installed {}, lit the last frame {}, {} probes, {} MiB on the GPU{})",
            if options.baked_gi.is_some() {
                "baked"
            } else {
                "live"
            },
            result.baked_gi.installed,
            result.baked_gi.active,
            result.baked_gi.probes,
            result.baked_gi.gpu_bytes / (1024 * 1024),
            result
                .baked_gi
                .error
                .as_deref()
                .map_or(String::new(), |e| format!(", error: {e}")),
        );
        if options.baked_gi.is_some() && !result.baked_gi.active {
            eprintln!("  warning: --baked-gi was given but the volume did not light the last frame");
        }
        if let Some(m) = result.motion.as_ref() {
            // The same numbers as `motion.json` in the capture directory.
            println!(
                "  motion: {} frames, flicker mean {:.3} p99 {:.3} (px >8 levels {:.4}), clarity {:.3}, \
                 smear {:.3}, still/native {:.3}, moving/native {:.3}, rotation flicker {:.3} ({} pairs), pops {}/{}",
                m.frames,
                m.flicker_mean,
                m.flicker_p99,
                m.flicker_px_over_8,
                m.clarity,
                m.smear,
                m.still_vs_native,
                m.moving_vs_native,
                m.rotation_flicker,
                m.rotation_pairs,
                m.pops,
                m.pop_samples
            );
            for c in &m.crops {
                println!(
                    "    crop {:?}: clarity {:.3} smear {:.3} still/native {:.3} moving/native {:.3} flicker {:.3}",
                    c.rect, c.clarity, c.smear, c.still_vs_native, c.moving_vs_native, c.flicker
                );
            }
        }
        let casters_p50 = lookdev::chase::percentile(
            &result
                .samples
                .iter()
                .map(|s| s.casters as f32)
                .collect::<Vec<_>>(),
            0.5,
        );
        for line in lookdev::chase::shadow_cache_lines(&result, casters_p50) {
            println!("{line}");
        }
        for (c, (jobs, full, region, ms)) in lookdev::chase::cascade_summary(&result).iter().enumerate() {
            println!(
                "  cascade {c}: {jobs:.2} shadow jobs/frame, terrain whole-cascade {full} frames, page-region {region} frames, mean {ms:.3} ms"
            );
        }
        for (name, p50, p99) in rows(&result) {
            println!("  {name:<20} p50 {p50:>7.3} ms   p99 {p99:>7.3} ms");
        }
        if let Some(n) = options.hold_after {
            // Measured sample j is loop frame warmup + j; frame n captures, later frames are held.
            let gpu = |keep: &dyn Fn(u32) -> bool| -> Vec<f32> {
                let mut v: Vec<f32> = (0u32..)
                    .zip(&result.samples)
                    .filter(|(j, _)| keep(options.warmup + j))
                    .map(|(_, s)| s.gpu.frame_ms)
                    .filter(|ms| *ms > 0.0)
                    .collect();
                v.sort_by(f32::total_cmp);
                v
            };
            let before = gpu(&|i| i < n);
            let held = gpu(&|i| i > n);
            let p50 = |v: &[f32]| regia_render::hitch::percentile_sorted(v, 0.5);
            println!(
                "  scene hold (from frame {n}): gpu_frame p50 before {:.3} ms ({} frames), while held {:.3} ms ({} frames)",
                p50(&before),
                before.len(),
                p50(&held),
                held.len()
            );
        }
        let report = result.hitch_report();
        let text =
            lookdev::soak::verdict_for_warmup(&lookdev::soak::report_text(&result, &report), options.warmup);
        println!("{text}");
        failed_gate |= options.soak.is_some() && options.warmup > 0 && !report.passes();
        if let Some(path) = hitch_csv.as_ref() {
            let out = if maps_len > 1 {
                path.with_extension(format!("{}.csv", result.map))
            } else {
                path.clone()
            };
            lookdev::soak_bundle::write_hitch_csv(&bundle, &out, &result.samples, report.p50_ms)?;
            println!("wrote {}", out.display());
        }
        if let Some(dir) = timeline_dir.as_ref() {
            lookdev::soak::write_timeline_files(dir, &result, &report, &text)?;
            println!(
                "wrote {}/{}.{{csv,png}} and -report.txt",
                dir.display(),
                result.map
            );
        }
        for (i, s) in result.samples.iter().enumerate() {
            let g = s.gpu;
            // Terrain virtual texture columns (zeros when it is off): a page miss shades in
            // full, so `vt_oldest_miss_frames` is how long the slowest request waited, not a
            // visible pop.
            let vt = s.vt.unwrap_or_default();
            csv.push_str(&format!(
                "{},{i},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{:.3},{:.3},{:.3},{},{},{},{},{:.4},{:.3},{:.3},{},{}\n",
                result.map,
                s.wall_ms,
                s.cpu_ms,
                s.encode_ms,
                s.submit_ms,
                s.scatter_ms,
                g.frame_ms,
                g.shadows_ms,
                g.clusters_ms,
                g.prepass_ms,
                g.cull_ms,
                g.ssao_ms,
                g.forward_ms,
                g.ssgi_ms,
                g.ssr_ms,
                g.atmosphere_ms,
                g.clouds_ms,
                g.fog_ms,
                g.taau_ms,
                g.post_ms,
                s.rocks,
                g.shared_forward_ms,
                vt.bake_ms,
                vt.reduce_ms,
                vt.baked,
                vt.rebaked,
                vt.missing,
                vt.oldest_miss_frames,
                vt.hit_rate,
                g.sssr_trace_ms,
                g.sssr_denoise_ms,
                s.sssr.ray_quads,
                s.sssr.tiles,
            ));
        }
    }
    if let Some(path) = value(args, "--csv") {
        std::fs::File::create(path)
            .and_then(|mut f| f.write_all(csv.as_bytes()))
            .map_err(|source| LookdevError::Io {
                path: path.to_string(),
                source,
            })?;
        println!("wrote {path}");
    }
    if let Some(keeper) = keeper.as_ref() {
        let k = keeper.stats();
        println!(
            "residency keeper: {} sweeps over {} MiB of data and {} MiB of code; {} pages found paged out and read back; longest sweep {} us",
            k.sweeps,
            k.data_bytes >> 20,
            k.code_bytes >> 20,
            k.restored_pages,
            k.max_sweep_us
        );
    }
    if let Some(path) = summary_path.as_deref() {
        lookdev::session_summary::write_run(
            path,
            &lookdev::session_summary::RunRecord {
                scenario,
                window: windowed,
                maps: maps_run,
                width: options.width,
                height: options.height,
                wall_ms: walls,
                prewarm_ms,
                cache: cache_label,
                gpu: gpu_name,
                backend,
                content: content.display().to_string(),
                clean: !failed_gate,
                panic: false,
                note: if failed_gate {
                    "the soak missed specs/27 § 2.4".to_owned()
                } else {
                    String::new()
                },
            },
        )?;
    }
    if failed_gate {
        return Err(LookdevError::Config(
            "the soak missed specs/27 § 2.4 (see the verdicts above)".into(),
        ));
    }
    if late_pipelines > 0 {
        return Err(LookdevError::Config(format!(
            "--assert-no-late-pipelines: {late_pipelines} pipeline(s) created after warm-up (listed above)"
        )));
    }
    Ok(())
}

/// `--assert-no-late-pipelines`: prints every pipeline the measured frames of `map` created
/// (label, target format, sample count, thread, creation site, new forward-set key axes;
/// ADR-7885) and returns how many there were (`pipelines`, the counter; the trace names them).
/// `--prepare-cache`: `regia_render::prepare::run` into `dir` with one map per `maps`, whose job
/// is that map's chase (warm-up and one frame) on the prepare's device, at the chase's target
/// format. Prints the progress lines and the wall and worker time.
fn prepare_maps(
    dir: &std::path::Path,
    content: &std::path::Path,
    maps: &[Planet],
    options: &ChaseOptions,
) -> Result<(), LookdevError> {
    use regia_render::prepare::{self, PrepareMap, PrepareRequest};
    let mut request = PrepareRequest::new(dir);
    request.nice = false;
    request.timeout = std::time::Duration::from_secs(3600);
    for &planet in maps {
        let content = content.to_path_buf();
        let mut chase = options.clone();
        chase.frames = 1;
        chase.warmup = 0;
        chase.png = None;
        let name = lookdev::do27::map_name(planet);
        let map = PrepareMap::new(name, "do27_chase").with_job("chase", move |device, format| {
            // The chase draws into Rgba8UnormSrgb only.
            if format != wgpu::TextureFormat::Rgba8UnormSrgb {
                return Ok(());
            }
            let mut planets = R2Planets::default();
            lookdev::chase::run_with(device, &mut planets, &content, planet, &chase, &mut |_, _| {})
                .map(drop)
                .map_err(|err| err.to_string())
        });
        request.maps.push(map);
    }
    let report = prepare::run(&request, |progress| println!("{}", progress.to_json()))
        .map_err(|err| LookdevError::Config(format!("prepare: {err}")))?;
    println!(
        "prepare: {} pipelines, wall {} ms, worker time {} ms, {} workers, cache {}",
        report.pipelines,
        report.elapsed_ms,
        report.worker_ms,
        report.threads,
        report.cache_path.as_deref().unwrap_or("none")
    );
    Ok(())
}

fn late_pipeline_report(map: &str, pipelines: u64, events: &[(u32, regia_render::FramePathEvent)]) -> u64 {
    if pipelines == 0 {
        println!("  late pipelines ({map}): 0 after warm-up");
        return 0;
    }
    println!("  LATE PIPELINES ({map}): {pipelines} created after warm-up");
    for (frame, event) in events
        .iter()
        .filter(|(_, e)| e.kind == regia_render::FramePathKind::Pipeline)
    {
        let detail = &event.detail;
        let axes: Vec<&str> = detail.new_axes.names().collect();
        println!(
            "    frame {frame}: `{}` format {:?} samples {} depth {:?} thread {} site {} new key axes [{}]",
            event.label,
            detail.format,
            detail.samples,
            detail.depth_format,
            if detail.warm_worker {
                "warm_worker"
            } else {
                "frame"
            },
            detail
                .site
                .map_or_else(|| String::from("?"), |s| format!("{}:{}", s.file(), s.line())),
            axes.join(", "),
        );
    }
    pipelines
}

#[cfg(not(target_os = "android"))]
fn chase_job(
    device: &regia_render::RenderDevice,
    planets: &mut R2Planets,
    content: &std::path::Path,
    planet: Planet,
    options: &ChaseOptions,
    presenter: Option<&mut dyn lookdev::chase::FramePresent>,
    after_frame: &mut dyn FnMut(u32, &regia_render::FrameRenderer),
) -> Result<lookdev::chase::ChaseRun, LookdevError> {
    lookdev::chase::run_with_present(device, planets, content, planet, options, after_frame, presenter)
}

/// N frames that clear a 64² target and copy 64 KiB through `write_buffer` (a frame's
/// smallest shape), two in flight: the percentiles of `queue.submit` and of the frame.
fn submit_probe(device: &regia_render::RenderDevice, frames: u32) -> Result<(), LookdevError> {
    let d = &device.device;
    let target = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe target"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let buffer = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe upload"),
        size: 65_536,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::VERTEX,
        mapped_at_creation: false,
    });
    let bytes = vec![7u8; 65_536];
    let mut in_flight = std::collections::VecDeque::new();
    let (mut submit, mut frame) = (Vec::new(), Vec::new());
    for _ in 0..frames {
        let started = std::time::Instant::now();
        device.queue.write_buffer(&buffer, 0, &bytes);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("probe clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        }));
        let t = std::time::Instant::now();
        in_flight.push_back(device.queue.submit([encoder.finish()]));
        submit.push(t.elapsed().as_secs_f32() * 1000.0);
        if in_flight.len() >= 3
            && let Some(oldest) = in_flight.pop_front()
        {
            d.poll(wgpu::PollType::Wait {
                submission_index: Some(oldest),
                timeout: None,
            })
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        }
        frame.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    let floor = lookdev::soak::cpu_floor(frames);
    println!("{}", floor.summary());
    for (name, mut v) in [("submit", submit), ("frame", frame)] {
        v.sort_by(f32::total_cmp);
        let q = |p: f32| lookdev::chase::percentile(&v, p);
        let over = |ms: f32| v.iter().filter(|x| **x > ms).count();
        println!(
            "probe {name}: p50 {:.3} p99 {:.3} ({:.2}x) p99.9 {:.3} ({:.2}x) max {:.2} ms; > 2 x p50: {}, > 5 ms: {}, > 20 ms: {}",
            q(0.5),
            q(0.99),
            q(0.99) / q(0.5).max(1e-6),
            q(0.999),
            q(0.999) / q(0.5).max(1e-6),
            v.last().copied().unwrap_or(0.0),
            over(2.0 * q(0.5)),
            over(5.0),
            over(20.0)
        );
    }
    Ok(())
}
