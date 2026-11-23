// SPDX-License-Identifier: MIT-0
//! DO27's frame-time bench on its real maps (`do27_bench`, `cargo xtask bench-do27`).
//!
//! Each case (map × tier × output size) runs the shell's frame ([`crate::chase`]) along a
//! fixed camera path in three segments, each measured on its own run after a warm-up:
//!
//! - **spawn**: the chase camera behind the vehicle driving away from the team-1 start;
//! - **flyover**: the eye 60 m above the route moving at 60 m/s, pitched 15° down toward the
//!   route ahead (the horizon and the far field in view);
//! - **fight**: the vehicle parked half-way along the route, the chase camera orbiting it.
//!
//! Every measured frame records the GPU pass timestamps ([`regia_render::GpuTimings`]),
//! the renderer's CPU phases, the bench's host-side scatter work and, through a `tracing`
//! layer ([`crate::spans::install_systems`]), the CPU time of every span two levels below
//! the frame's roots on every thread. The bench prints per case a pass table (p50 / p95 /
//! p99 per segment and over the whole path), the costliest CPU systems and the five
//! largest costs, and writes `summary.csv` (one row per case, segment and metric) for a
//! later `--baseline` comparison.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use regia_render::RenderProfilePreset;

use crate::chase::{ChaseOptions, ChaseRun, columns, percentile, route};
use crate::dense_scatter::BenchScatter;
use crate::look_r2::R2Planets;
use crate::motion::Motion;
use crate::soak::polyline_length;
use crate::spans::SystemSpans;
use crate::{LookdevError, Planet};

/// Owner play resolution, measured beside 1440p and 4K. Not a replacement for either.
pub const ULTRAWIDE: (u32, u32) = (3840, 1600);

/// One camera path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// The chase camera from the team-1 start.
    Spawn,
    /// 60 m above the route at 60 m/s.
    Flyover,
    /// The vehicle parked half-way along the route, the camera orbiting it.
    Fight,
}

impl Segment {
    /// All three, in path order.
    pub const ALL: [Self; 3] = [Self::Spawn, Self::Flyover, Self::Fight];

    /// Name in tables and CSV.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Flyover => "flyover",
            Self::Fight => "fight",
        }
    }

    /// Parses a name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Route start (metres) and camera motion of the segment on a route `route_m` long.
    #[must_use]
    pub fn path(self, route_m: f32) -> (f32, Motion) {
        match self {
            Self::Spawn => (0.0, Motion::Chase),
            Self::Flyover => (
                0.0,
                Motion::Flyover {
                    height_m: 60.0,
                    m_per_frame: 1.0,
                },
            ),
            Self::Fight => (route_m * 0.5, Motion::Orbit { deg_per_frame: 0.6 }),
        }
    }
}

/// A render tier of the bench.
#[must_use]
pub fn tier_name(preset: RenderProfilePreset) -> &'static str {
    match preset {
        RenderProfilePreset::DesktopUltra => "ultra",
        RenderProfilePreset::DesktopHigh => "high",
        RenderProfilePreset::DesktopLow => "low",
        RenderProfilePreset::Deck => "deck",
        RenderProfilePreset::MobileHigh => "mobile-high",
        RenderProfilePreset::MobileLow => "mobile-low",
        RenderProfilePreset::Cinematic => "cinematic",
    }
}

/// Parses `high`, `ultra`, `low`, `deck`, `mobile-high`, `mobile-low`.
#[must_use]
pub fn parse_tier(name: &str) -> Option<RenderProfilePreset> {
    [
        RenderProfilePreset::DesktopUltra,
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::Deck,
        RenderProfilePreset::MobileHigh,
        RenderProfilePreset::MobileLow,
    ]
    .into_iter()
    .find(|p| tier_name(*p) == name)
}

/// The bench's configuration.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// Maps.
    pub maps: Vec<Planet>,
    /// Tiers.
    pub tiers: Vec<RenderProfilePreset>,
    /// Output sizes.
    pub sizes: Vec<(u32, u32)>,
    /// Path segments.
    pub segments: Vec<Segment>,
    /// Measured frames per segment.
    pub frames: u32,
    /// Warm-up frames per segment.
    pub warmup: u32,
    /// Write each segment's last frame as `<map>_<tier>_<w>x<h>_<segment>.png` here.
    pub png_dir: Option<PathBuf>,
    /// Passes off (`ChaseOptions::off`), for a diagnosis.
    pub off: Vec<String>,
    /// Opt-in levers a host may turn on (`None`/`OFF` = the tier's defaults): the terrain
    /// virtual texture (ADR-117), the terrain budget cuts, a render scale and upscaler.
    pub vt: Option<regia_terrain::VirtualTextureSettings>,
    /// The terrain budget cuts (`TerrainBudget::OFF` = the default look).
    pub budget: regia_render::TerrainBudget,
    /// The terrain's anisotropic filtering level (`None` = the tier's).
    pub terrain_anisotropy: Option<u8>,
    /// Internal render scale (`None` = the tier's).
    pub render_scale: Option<f32>,
    /// Temporal upscaler.
    pub upscaler: regia_render::TemporalUpscaler,
    /// The scatter set drawn (`None` = the map's `scatter.ron` as loaded, the default).
    pub scatter: Option<BenchScatter>,
    /// The scatter's opt-in detail limits ([`regia_terrain::ScatterDetail`]; `None` = off, the
    /// default frame). The reference resolution is set to each case's output height and the
    /// chase's field of view.
    pub detail: Option<regia_terrain::ScatterDetail>,
    /// Log every frame-path create after warm-up by label (`ChaseOptions::trace`).
    pub trace: bool,
    /// Content-side density levers (coarse LODs, clumps) for every scatter variant.
    pub levers: crate::dense_scatter::DensityLevers,
    /// Scatter density factors (`1` = the set as authored); each is its own case
    /// ([`crate::dense_scatter::densify`]).
    pub density: Vec<f32>,
    /// Drive `regia_telemetry` every frame as a game does (`REGIA_TELEMETRY=normal|verbose`):
    /// the frame profiler it reads hitch spans from. Its per-frame cost is printed per segment.
    pub telemetry: Option<regia_core::profiling::FrameProfiler>,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            maps: vec![Planet::Europa, Planet::Mars],
            tiers: vec![
                RenderProfilePreset::DesktopHigh,
                RenderProfilePreset::DesktopUltra,
            ],
            sizes: vec![(2560, 1440), ULTRAWIDE, (3840, 2160)],
            segments: Segment::ALL.to_vec(),
            frames: 600,
            warmup: 120,
            png_dir: None,
            off: Vec::new(),
            vt: None,
            budget: regia_render::TerrainBudget::OFF,
            terrain_anisotropy: None,
            render_scale: None,
            upscaler: regia_render::TemporalUpscaler::Taau,
            scatter: None,
            detail: None,
            levers: crate::dense_scatter::DensityLevers::default(),
            trace: false,
            density: vec![1.0],
            telemetry: None,
        }
    }
}

/// One measured segment.
#[derive(Debug, Clone)]
pub struct SegmentRun {
    /// The segment.
    pub segment: Segment,
    /// The chase run.
    pub run: ChaseRun,
    /// Per measured frame: CPU ms by span chain ([`SystemSpans::take_frame`]).
    pub systems: Vec<HashMap<String, f64>>,
}

/// One case: a map at a tier and size, every segment.
#[derive(Debug, Clone)]
pub struct CaseRun {
    /// Map name, with the scatter variant when it is not the default
    /// (`switchbacks+scan-x10`: the scan set at 10× density).
    pub map: String,
    /// Tier name.
    pub tier: &'static str,
    /// Output size.
    pub size: (u32, u32),
    /// The segments, in path order.
    pub segments: Vec<SegmentRun>,
    /// The scatter drawn: resident instances and GPU memory at the end of each segment.
    pub scatter: CaseScatter,
}

/// What a case's scatter held and cost in memory.
#[derive(Debug, Clone, Default)]
pub struct CaseScatter {
    /// Expected candidates per square metre before the filters.
    pub candidates_per_m2: f32,
    /// Instances resident in the field at the end of each segment.
    pub resident: Vec<u32>,
    /// GPU memory at the end of each segment: (all live allocations, scatter, meshes), bytes.
    pub gpu_bytes: Vec<(u64, u64, u64)>,
    /// Process resident set at the end of each segment, bytes.
    pub rss_bytes: Vec<u64>,
}

/// Suffix naming the scatter levers on (`+detail`, `+lod`, `+clump8`), empty when none.
#[must_use]
pub fn lever_tags(options: &BenchOptions) -> String {
    let mut tags = String::new();
    if options.detail.is_some() {
        tags.push_str("+detail");
    }
    if options.levers.coarse_lod {
        tags.push_str("+lod");
    }
    if options.levers.clump > 1 {
        tags.push_str(&format!("+clump{}", options.levers.clump));
    }
    if options.levers.shared_materials {
        tags.push_str("+shared");
    }
    tags
}

/// `map` for a scatter variant: unchanged for the default set at 1×, else
/// `<map>+<set>-x<factor>`.
#[must_use]
pub fn variant_name(map: &str, scatter: Option<BenchScatter>, factor: f32) -> String {
    match (scatter, factor) {
        (None, f) if f <= 1.0 => map.to_owned(),
        (Some(s), f) if f <= 1.0 => format!("{map}+{}", s.name()),
        (s, f) => format!(
            "{map}+{}-x{f}",
            s.map_or(BenchScatter::Map.name(), BenchScatter::name)
        ),
    }
}

/// One summary row: a metric's percentiles over a segment (or `all`).
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryRow {
    /// `map,tier,WxH` of the case.
    pub case: String,
    /// Segment name or `all`.
    pub segment: String,
    /// Metric (a pass, a CPU phase, `sys <span chain>`).
    pub metric: String,
    /// p50, ms.
    pub p50: f32,
    /// p95, ms.
    pub p95: f32,
    /// p99, ms.
    pub p99: f32,
    /// Mean, ms.
    pub mean: f32,
}

/// Telemetry driven by hand inside the bench loop, with its own cost measured per frame.
struct TelemetryProbe {
    telemetry: regia_telemetry::Telemetry,
    profiler: regia_core::profiling::FrameProfiler,
    start: std::time::Instant,
    last: Option<std::time::Instant>,
    /// Measured frames: (cost µs, the frame detected a hitch, the frame emitted a hitch event).
    costs: Vec<(f32, bool, bool)>,
    /// Span enter/exit pairs the profiler recorded per measured frame.
    spans: Vec<u64>,
}

impl TelemetryProbe {
    fn new(profiler: regia_core::profiling::FrameProfiler) -> Self {
        Self {
            telemetry: regia_telemetry::Telemetry::new(
                regia_telemetry::TelemetryOptions::default().with_env(),
            ),
            profiler,
            start: std::time::Instant::now(),
            last: None,
            costs: Vec::with_capacity(4096),
            spans: Vec::with_capacity(4096),
        }
    }

    fn frame(&mut self, measured: bool, renderer: &regia_render::FrameRenderer) {
        let now = std::time::Instant::now();
        let frame_ms = self
            .last
            .map_or(0.0, |l| now.duration_since(l).as_secs_f32() * 1000.0);
        self.last = Some(now);
        let before = self.telemetry.counts();
        // What a game pays per frame: the profiler closes the frame, telemetry reads it.
        self.profiler.end_frame();
        self.telemetry.observe_frame(
            frame_ms,
            now.duration_since(self.start).as_secs_f64(),
            Some(renderer),
            Some(&self.profiler),
        );
        let cost = now.elapsed().as_secs_f32() * 1e6;
        let after = self.telemetry.counts();
        if measured {
            self.spans.push(self.profiler.last_frame_span_count());
            self.costs.push((
                cost,
                after.hitches > before.hitches,
                after.hitch_events > before.hitch_events,
            ));
        }
    }

    fn report(&self) -> String {
        let stat = |sel: &dyn Fn(&(f32, bool, bool)) -> bool| {
            let mut v: Vec<f32> = self.costs.iter().filter(|c| sel(c)).map(|c| c.0).collect();
            v.sort_by(f32::total_cmp);
            if v.is_empty() {
                return "none".to_owned();
            }
            let mean = v.iter().sum::<f32>() / v.len() as f32;
            let at = |p: f32| v[((v.len() - 1) as f32 * p).round() as usize];
            format!(
                "n {} mean {mean:.1} p50 {:.1} p95 {:.1} max {:.1} us",
                v.len(),
                at(0.5),
                at(0.95),
                at(1.0)
            )
        };
        let c = self.telemetry.counts();
        format!(
            "spans/frame {:.0}; telemetry cost: all frames [{}]; hitch-detect frames [{}]; hitch-event frames [{}]; frames without events [{}]; hitches {} events {} summaries {}",
            self.spans.iter().sum::<u64>() as f64 / self.spans.len().max(1) as f64,
            stat(&|_| true),
            stat(&|c| c.1),
            stat(&|c| c.2),
            stat(&|c| !c.1 && !c.2),
            c.hitches,
            c.hitch_events,
            c.summaries
        )
    }
}

/// Runs every case.
///
/// # Errors
///
/// Content loading or a frame failed.
pub fn run(
    device: &regia_render::RenderDevice,
    content: &Path,
    options: &BenchOptions,
    systems: Option<&SystemSpans>,
    mut on_case: impl FnMut(&CaseRun),
) -> Result<Vec<CaseRun>, LookdevError> {
    let mut planets = R2Planets::default();
    let mut cases = Vec::new();
    for &planet in &options.maps {
        let route_m = polyline_length(&route(content, planet)?);
        for &factor in &options.density {
            let mut density = CaseScatter::default();
            let reserve = if options.scatter.is_some()
                || factor > 1.0
                || options.levers != crate::dense_scatter::DensityLevers::default()
            {
                let terrain = planets.get(planet, 1025)?;
                let source = options.scatter.unwrap_or(BenchScatter::Map);
                let set =
                    crate::dense_scatter::install(content, planet, terrain, source, factor, options.levers)?;
                density.candidates_per_m2 = crate::dense_scatter::candidates_per_m2(&set);
                // The per-instance buffers sized for the variant at level load (no growth on
                // the frame path).
                // Clumps place a fraction of the instances (≤ 12 k per frame at 10× on Switchbacks).
                let scale = if options.levers.clump > 1 {
                    (factor / 4.0).ceil()
                } else {
                    factor
                };
                crate::chase::RESERVE_INSTANCES.max((crate::chase::RESERVE_INSTANCES as f32 * scale) as u32)
            } else {
                crate::chase::RESERVE_INSTANCES
            };
            for &tier in &options.tiers {
                for &(width, height) in &options.sizes {
                    let mut case = CaseRun {
                        map: format!(
                            "{}{}",
                            variant_name(crate::do27::map_name(planet), options.scatter, factor),
                            lever_tags(options)
                        ),
                        tier: tier_name(tier),
                        size: (width, height),
                        segments: Vec::new(),
                        scatter: density.clone(),
                    };
                    if let Some(field) = planets.get(planet, 1025)?.scatter.as_mut() {
                        let aspect = width as f32 / height.max(1) as f32;
                        field.set_detail(options.detail.map(|d| regia_terrain::ScatterDetail {
                            reference_height_px: height as f32,
                            reference_fov_y: 2.0 * ((90.0f32.to_radians() * 0.5).tan() / aspect).atan(),
                            ..d
                        }));
                    }
                    for &segment in &options.segments {
                        let (start_m, motion) = segment.path(route_m);
                        let chase = ChaseOptions {
                            width,
                            height,
                            frames: options.frames,
                            warmup: options.warmup,
                            start_m,
                            motion,
                            preset: Some(tier),
                            off: options.off.clone(),
                            vt: options.vt,
                            budget: options.budget,
                            terrain_anisotropy: options.terrain_anisotropy,
                            render_scale: options.render_scale,
                            upscaler: options.upscaler,
                            reserve_instances: reserve,
                            trace: options.trace,
                            png: options.png_dir.as_ref().map(|dir| {
                                dir.join(format!(
                                    "{}_{}_{width}x{height}_{}.png",
                                    case.map,
                                    case.tier,
                                    segment.name()
                                ))
                            }),
                            ..ChaseOptions::default()
                        };
                        let mut frames = Vec::with_capacity(options.frames as usize);
                        if let Some(systems) = systems {
                            let _ = systems.take_frame();
                        }
                        let warmup = options.warmup;
                        let last = options.warmup + options.frames.saturating_sub(1);
                        let mut memory = None;
                        let mut telemetry = options
                            .telemetry
                            .as_ref()
                            .map(|profiler| TelemetryProbe::new(profiler.clone()));
                        let run = crate::chase::run_with(
                            device,
                            &mut planets,
                            content,
                            planet,
                            &chase,
                            &mut |i, renderer| {
                                if let Some(probe) = telemetry.as_mut() {
                                    probe.frame(i >= warmup, renderer);
                                }
                                if let Some(systems) = systems {
                                    let totals = systems.take_frame();
                                    if i >= warmup {
                                        frames.push(totals);
                                    }
                                }
                                if i == last {
                                    memory = Some(regia_render::memory_report(device));
                                }
                            },
                        )?;
                        if let Some(probe) = telemetry.as_ref() {
                            println!(
                                "{} {} {}x{} {}: {}",
                                case.map,
                                case.tier,
                                width,
                                height,
                                segment.name(),
                                probe.report()
                            );
                        }
                        if let Some(report) = memory {
                            use regia_render::MemoryCategory as M;
                            case.scatter.gpu_bytes.push((
                                report.allocated_bytes,
                                report.bytes(M::Scatter),
                                report.bytes(M::Meshes),
                            ));
                            case.scatter.rss_bytes.push(report.rss_bytes.unwrap_or(0));
                        }
                        let resident = planets
                            .get(planet, 1025)?
                            .scatter
                            .as_ref()
                            .map_or(0, |f| f.len() as u32);
                        case.scatter.resident.push(resident);
                        case.segments.push(SegmentRun {
                            segment,
                            run,
                            systems: frames,
                        });
                    }
                    on_case(&case);
                    cases.push(case);
                }
            }
        }
    }
    Ok(cases)
}

fn stats(name: &str, case: &str, segment: &str, values: &[f32]) -> SummaryRow {
    let mean = values.iter().sum::<f32>() / values.len().max(1) as f32;
    SummaryRow {
        case: case.to_owned(),
        segment: segment.to_owned(),
        metric: name.to_owned(),
        p50: percentile(values, 0.5),
        p95: percentile(values, 0.95),
        p99: percentile(values, 0.99),
        mean,
    }
}

impl CaseRun {
    /// `map,tier,WxH`.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{},{},{}x{}", self.map, self.tier, self.size.0, self.size.1)
    }

    /// Every metric per segment and over the whole path (`all`): the pass columns
    /// ([`columns`]), then `sys <span chain>` for every span seen.
    #[must_use]
    pub fn summary(&self) -> Vec<SummaryRow> {
        let key = self.key();
        let mut out = Vec::new();
        let mut all: Vec<(&'static str, Vec<f32>)> = Vec::new();
        let mut span_names: Vec<String> = self
            .segments
            .iter()
            .flat_map(|s| s.systems.iter().flat_map(|f| f.keys().cloned()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        span_names.sort();
        let mut all_spans: Vec<Vec<f32>> = vec![Vec::new(); span_names.len()];
        for seg in &self.segments {
            for (i, (name, values)) in columns(&seg.run)
                .into_iter()
                .chain(host_columns(&seg.run))
                .enumerate()
            {
                out.push(stats(name, &key, seg.segment.name(), &values));
                if all.len() <= i {
                    all.push((name, Vec::new()));
                }
                all[i].1.extend(values);
            }
            for (i, name) in span_names.iter().enumerate() {
                let values: Vec<f32> = seg
                    .systems
                    .iter()
                    .map(|f| f.get(name).copied().unwrap_or(0.0) as f32)
                    .collect();
                if !values.is_empty() {
                    out.push(stats(&format!("sys {name}"), &key, seg.segment.name(), &values));
                    all_spans[i].extend(values);
                }
            }
        }
        for (name, values) in all {
            out.push(stats(name, &key, "all", &values));
        }
        for (name, values) in span_names.iter().zip(all_spans) {
            if !values.is_empty() {
                out.push(stats(&format!("sys {name}"), &key, "all", &values));
            }
        }
        out
    }
}

/// The host's contention per measured frame: the frame thread's and the encode workers'
/// run-queue wait, the process cgroup's CPU pressure stall, and the core canary (µs; its
/// median is the core running alone). Frames whose CPU time grew with these are the host's
/// load, not the engine.
fn host_columns(run: &ChaseRun) -> Vec<(&'static str, Vec<f32>)> {
    let col =
        |f: &dyn Fn(&crate::chase::ChaseSample) -> f32| -> Vec<f32> { run.samples.iter().map(f).collect() };
    vec![
        // The frame thread's CPU with its run-queue wait taken out (approximate: the wait is
        // measured over the whole frame iteration, so this can under-read a busy frame).
        (
            "cpu render_into - runq",
            col(&|s| (s.cpu_ms - s.runq_ms).max(0.0)),
        ),
        ("host runq frame thread", col(&|s| s.runq_ms)),
        ("host runq encode workers", col(&|s| s.runq_workers_ms)),
        ("host psi cpu", col(&|s| s.psi_cpu_ms)),
        ("host canary (us)", col(&|s| s.canary_us)),
        ("wait for gpu", col(&|s| s.wait_ms)),
    ]
}

/// GPU leaf passes (no totals, no counts) of [`columns`]: what the top-cost list ranks.
const GPU_LEAVES: [&str; 18] = [
    "  cascade 0",
    "  cascade 1",
    "  cascade 2",
    "  cascade 3",
    "  local atlas",
    "gpu clusters",
    "gpu prepass",
    "gpu cull",
    "gpu ssao",
    "gpu gi",
    "  terrain (in forward)",
    "  meshes+sky (forward-terrain)",
    "gpu ssgi",
    "gpu ssr",
    "gpu atmosphere",
    "gpu clouds",
    "gpu fog",
    "gpu taau",
];

/// The printed report of one case: the pass table, the CPU systems, the top five costs.
#[must_use]
pub fn case_text(case: &CaseRun, rows: &[SummaryRow]) -> String {
    let mut text = String::new();
    let segs: Vec<&str> = case
        .segments
        .iter()
        .map(|s| s.segment.name())
        .chain(std::iter::once("all"))
        .collect();
    let frames: usize = case.segments.iter().map(|s| s.run.samples.len()).sum();
    let _ = writeln!(
        text,
        "\n== {} {} {}x{}: {} segments, {frames} measured frames ==",
        case.map,
        case.tier,
        case.size.0,
        case.size.1,
        case.segments.len()
    );
    let s = &case.scatter;
    let mb = |b: u64| b as f64 / 1.0e6;
    let gpu = s.gpu_bytes.iter().copied().max().unwrap_or_default();
    let creates: u64 = case
        .segments
        .iter()
        .map(|s| {
            s.run.counters.pipelines
                + s.run.counters.bind_groups
                + s.run.counters.buffers
                + s.run.counters.textures
        })
        .sum();
    let _ = writeln!(
        text,
        "scatter: {:.3} candidates/m2, resident {:?}; GPU memory {:.1} MB (scatter {:.1}, meshes {:.1}), RSS {:.0} MB; frame-path creates after warm-up {creates}",
        s.candidates_per_m2,
        s.resident,
        mb(gpu.0),
        mb(gpu.1),
        mb(gpu.2),
        mb(s.rss_bytes.iter().copied().max().unwrap_or(0)),
    );
    for seg in &case.segments {
        for (frame, event) in &seg.run.events {
            let _ = writeln!(
                text,
                "  create after warm-up: {} frame {frame}: {:?} `{}` ({} bytes)",
                seg.segment.name(),
                event.kind,
                event.label,
                event.bytes
            );
        }
    }
    let mut header = format!("{:<32}", "ms (p50 / p95 / p99)");
    for s in &segs {
        let _ = write!(header, " | {s:^20}");
    }
    let _ = writeln!(text, "{header}");
    let by: HashMap<(&str, &str), &SummaryRow> = rows
        .iter()
        .map(|r| ((r.segment.as_str(), r.metric.as_str()), r))
        .collect();
    let metrics: Vec<&str> = rows
        .iter()
        .filter(|r| r.segment == "all" && !r.metric.starts_with("sys "))
        .map(|r| r.metric.as_str())
        .collect();
    for metric in &metrics {
        let mut line = format!("{metric:<32}");
        for s in &segs {
            match by.get(&(*s, *metric)) {
                Some(r) => {
                    let _ = write!(line, " | {:>6.2} {:>6.2} {:>6.2}", r.p50, r.p95, r.p99);
                }
                None => line.push_str(" |                     "),
            }
        }
        let _ = writeln!(text, "{line}");
    }
    // CPU systems: the spans with the largest mean over the path.
    let mut spans: Vec<&SummaryRow> = rows
        .iter()
        .filter(|r| r.segment == "all" && r.metric.starts_with("sys "))
        .collect();
    spans.sort_by(|a, b| b.mean.total_cmp(&a.mean));
    if !spans.is_empty() {
        let _ = writeln!(text, "CPU systems (tracing spans, all threads; top 30 by mean):");
        for r in spans.iter().take(30) {
            let _ = writeln!(
                text,
                "  {:<58} mean {:>6.3}  p50 {:>6.3}  p95 {:>6.3}  p99 {:>6.3}",
                &r.metric[4..],
                r.mean,
                r.p50,
                r.p95,
                r.p99
            );
        }
    }
    let _ = writeln!(text, "top 5 costs (p50 over the path):");
    for (name, ms) in top_costs(rows).iter().take(5) {
        let _ = writeln!(text, "  {ms:>6.3} ms  {name}");
    }
    text
}

/// GPU leaf passes and the frame thread's CPU phases ranked by their p50 over the path.
#[must_use]
pub fn top_costs(rows: &[SummaryRow]) -> Vec<(String, f32)> {
    let mut costs: Vec<(String, f32)> = rows
        .iter()
        .filter(|r| r.segment == "all")
        .filter_map(|r| {
            if GPU_LEAVES.contains(&r.metric.as_str()) {
                Some((
                    format!("gpu {}", r.metric.trim().trim_start_matches("gpu ")),
                    r.p50,
                ))
            } else if matches!(
                r.metric.as_str(),
                "cpu encode" | "cpu submit" | "cpu scatter (host)"
            ) {
                Some((r.metric.clone(), r.p50))
            } else {
                None
            }
        })
        .collect();
    costs.sort_by(|a, b| b.1.total_cmp(&a.1));
    costs
}

/// The header of [`frames_csv`].
pub const FRAMES_HEADER: &str = "map,tier,size,segment,frame,wall,cpu,encode,submit,scatter,gpu_frame,gpu_forward,gpu_terrain_forward,gpu_ssgi,gpu_ssr,runq,runq_workers,psi_cpu,canary_us,wait,shadow_plan,draw_list,collect";

/// One row per measured frame of a case (`frames.csv`): the frame's times and its host
/// contention, to tell an engine spike from a preempted thread.
#[must_use]
pub fn frames_csv(case: &CaseRun) -> String {
    let mut out = String::new();
    for seg in &case.segments {
        for (i, s) in seg.run.samples.iter().enumerate() {
            let span = |key: &str| {
                seg.systems
                    .get(i)
                    .and_then(|f| f.get(key))
                    .copied()
                    .unwrap_or(0.0)
            };
            let g = s.gpu;
            let _ = writeln!(
                out,
                "{},{},{}x{},{},{i},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.1},{:.3},{:.3},{:.3},{:.3}",
                case.map,
                case.tier,
                case.size.0,
                case.size.1,
                seg.segment.name(),
                s.wall_ms,
                s.cpu_ms,
                s.encode_ms,
                s.submit_ms,
                s.scatter_ms,
                g.frame_ms,
                g.forward_ms,
                g.shared_forward_ms,
                g.ssgi_ms,
                g.ssr_ms,
                s.runq_ms,
                s.runq_workers_ms,
                s.psi_cpu_ms,
                s.canary_us,
                s.wait_ms,
                span("render_into>encode>shadow_plan"),
                span("render_into>encode>draw_list"),
                span("collect"),
            );
        }
    }
    out
}

/// The CSV header of [`to_csv`].
pub const CSV_HEADER: &str = "map,tier,size,segment,metric,p50,p95,p99,mean";

/// `summary.csv`: one row per case, segment and metric.
#[must_use]
pub fn to_csv(rows: &[SummaryRow]) -> String {
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for r in rows {
        let _ = writeln!(
            out,
            "{},{},\"{}\",{:.4},{:.4},{:.4},{:.4}",
            r.case,
            r.segment,
            r.metric.trim(),
            r.p50,
            r.p95,
            r.p99,
            r.mean
        );
    }
    out
}

/// Parses [`to_csv`]'s output.
///
/// # Errors
///
/// A row does not have nine fields or a number does not parse.
pub fn from_csv(text: &str) -> Result<Vec<SummaryRow>, LookdevError> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || LookdevError::Config(format!("summary csv line {}: `{line}`", n + 1));
        // The metric is quoted; nothing else holds a comma or a quote.
        let (head, rest) = line.split_once(",\"").ok_or_else(bad)?;
        let (metric, nums) = rest.split_once("\",").ok_or_else(bad)?;
        let head: Vec<&str> = head.split(',').collect();
        let nums: Vec<f32> = nums
            .split(',')
            .map(|v| v.parse::<f32>().map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        let ([map, tier, size, segment], [p50, p95, p99, mean]) = (head.as_slice(), nums.as_slice()) else {
            return Err(bad());
        };
        out.push(SummaryRow {
            case: format!("{map},{tier},{size}"),
            segment: (*segment).to_owned(),
            metric: metric.to_owned(),
            p50: *p50,
            p95: *p95,
            p99: *p99,
            mean: *mean,
        });
    }
    Ok(out)
}

/// Metrics a baseline comparison checks (whole path). The GPU frame is the guard that holds
/// on a shared machine; the CPU and wall rows move with the host's load, so they only count
/// when neither run was contended ([`CONTENDED_RUNQ_P95_MS`]). The raw `cpu render_into`
/// counts only when one side lacks `cpu render_into - runq`. Even uncontended, SMT and cache
/// sharing move CPU times by tens of percent: certify CPU on a quiet host.
pub const GUARDED: [&str; 5] = [
    "gpu frame",
    "cpu render_into - runq",
    "cpu render_into",
    "cpu scatter (host)",
    "wall",
];

/// A run whose frame thread waited longer than this on a run queue at p95 (ms) ran on a
/// contended host: its CPU and wall percentiles are the machine's, not the engine's.
pub const CONTENDED_RUNQ_P95_MS: f32 = 0.5;

/// Compares `rows` with a `baseline` on [`GUARDED`] over the whole path: one line per case
/// and metric, and whether any p50 or p95 grew by more than `tolerance` (0.1 = 10 %) and
/// more than 0.1 ms. CPU and wall rows of a case where either run was contended print
/// `(host contended)` and do not count.
#[must_use]
pub fn compare(rows: &[SummaryRow], baseline: &[SummaryRow], tolerance: f32) -> (String, bool) {
    let index = |set: &'_ [SummaryRow]| -> BTreeMap<(String, String), SummaryRow> {
        set.iter()
            .filter(|r| r.segment == "all")
            .map(|r| ((r.case.clone(), r.metric.trim().to_owned()), r.clone()))
            .collect()
    };
    let (now, base) = (index(rows), index(baseline));
    let contended = |set: &BTreeMap<(String, String), SummaryRow>, case: &str| {
        set.get(&(case.to_owned(), "host runq frame thread".to_owned()))
            .is_some_and(|r| r.p95 > CONTENDED_RUNQ_P95_MS)
    };
    let mut text = String::new();
    let mut regressed = false;
    let mut cases: Vec<&str> = rows.iter().map(|r| r.case.as_str()).collect();
    cases.dedup();
    for case in cases {
        let noisy = contended(&now, case) || contended(&base, case);
        for metric in GUARDED {
            let key = (case.to_owned(), metric.to_owned());
            let (Some(r), Some(b)) = (now.get(&key), base.get(&key)) else {
                continue;
            };
            // The raw frame-thread CPU only guards a baseline without the corrected row.
            let corrected = (case.to_owned(), "cpu render_into - runq".to_owned());
            if metric == "cpu render_into" && now.contains_key(&corrected) && base.contains_key(&corrected) {
                continue;
            }
            let worse = |now: f32, was: f32| now > was * (1.0 + tolerance) && now - was > 0.1;
            let counts = metric == "gpu frame" || !noisy;
            let flag = counts && (worse(r.p50, b.p50) || worse(r.p95, b.p95));
            regressed |= flag;
            let _ = writeln!(
                text,
                "{case:<34} {metric:<22} p50 {:>6.2} -> {:>6.2} ({:+5.1} %)  p95 {:>6.2} -> {:>6.2} ({:+5.1} %){}",
                b.p50,
                r.p50,
                (r.p50 / b.p50.max(1e-6) - 1.0) * 100.0,
                b.p95,
                r.p95,
                (r.p95 / b.p95.max(1e-6) - 1.0) * 100.0,
                if flag {
                    "  REGRESSION"
                } else if !counts {
                    "  (host contended)"
                } else {
                    ""
                }
            );
        }
    }
    (text, regressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(case: &str, metric: &str, p50: f32, p95: f32) -> SummaryRow {
        SummaryRow {
            case: case.into(),
            segment: "all".into(),
            metric: metric.into(),
            p50,
            p95,
            p99: p95,
            mean: p50,
        }
    }

    #[test]
    fn summary_csv_round_trips() {
        let rows = vec![
            row("bore_twelve,high,2560x1440", "gpu frame", 4.25, 5.5),
            row("bore_twelve,high,2560x1440", "sys render_into>encode", 1.0, 2.0),
        ];
        let parsed = from_csv(&to_csv(&rows)).expect("parses");
        assert_eq!(parsed, rows);
    }

    #[test]
    fn compare_flags_only_real_growth() {
        let base = vec![
            row("m,high,1x1", "wall", 5.0, 6.0),
            row("m,high,1x1", "gpu frame", 0.5, 0.6),
        ];
        // wall p95 +20 % (> 0.1 ms): a regression; gpu frame +16 % but only 0.08 ms: noise.
        let now = vec![
            row("m,high,1x1", "wall", 5.0, 7.2),
            row("m,high,1x1", "gpu frame", 0.58, 0.65),
        ];
        let (text, regressed) = compare(&now, &base, 0.1);
        assert!(regressed);
        assert_eq!(text.matches("REGRESSION").count(), 1, "{text}");
        let (_, regressed) = compare(&base, &base, 0.1);
        assert!(!regressed);
        // A contended run: the wall growth no longer counts, the GPU frame still does.
        let mut busy = now.clone();
        busy.push(row("m,high,1x1", "host runq frame thread", 0.0, 3.0));
        let (text, regressed) = compare(&busy, &base, 0.1);
        assert!(!regressed, "{text}");
        assert!(text.contains("(host contended)"), "{text}");
        busy[1].p50 = 0.9;
        assert!(compare(&busy, &base, 0.1).1);
    }

    #[test]
    fn default_sizes_keep_1440p_and_4k_and_add_ultrawide() {
        let sizes = BenchOptions::default().sizes;
        assert!(sizes.contains(&(2560, 1440)), "{sizes:?}");
        assert!(sizes.contains(&(3840, 2160)), "{sizes:?}");
        assert!(sizes.contains(&ULTRAWIDE), "{sizes:?}");
        assert_eq!(sizes.len(), 3, "{sizes:?}");
    }

    #[test]
    fn segments_and_tiers_parse() {
        assert_eq!(Segment::parse("flyover"), Some(Segment::Flyover));
        assert_eq!(parse_tier("ultra"), Some(RenderProfilePreset::DesktopUltra));
        let (start, motion) = Segment::Fight.path(1000.0);
        assert!((start - 500.0).abs() < 1e-3);
        assert!(matches!(motion, Motion::Orbit { .. }));
    }
}
