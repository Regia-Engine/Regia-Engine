// SPDX-License-Identifier: MIT-0
//! `vfx_lab --deck-check`: the Steam Deck measurement run (lane `v020-steam-deck`).
//!
//! The windowed lab scene (lit ground, sky, VFX set cycling) at the `Deck` render profile and
//! 1280×800, plus a field of moving "unit" boxes and a scripted camera path, for a fixed time
//! (default 5 minutes). It records frame times, the GPU pass timings, the Deck's sensors
//! (`/sys/class/hwmon`, `/sys/class/power_supply`, the amdgpu clock and busy counters) every five
//! seconds, and screenshots at scripted times, then writes `deck_check.json` next to them. The
//! engine's own `summary.json` (spec 28: frame percentiles, hitches, exit timings) lands in the
//! log directory. Run `vfx_lab --deck-check` and both files end up in `--out`.
//!
//! The sensor readers take the sysfs root as a parameter, so the parsing is tested against a fake
//! tree on any machine. Everything sysfs offers is optional: a missing file is a `null`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glam::Vec3;
use regia::app::{FramePacing, RealTime};
use regia::prelude::*;
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer, RuntimeRenderer, ScreenshotRequests};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// How often the sensors are sampled.
pub const SENSOR_PERIOD: Duration = Duration::from_secs(5);

/// `deck_tick` calls whose `RealTime::raw_delta` is not a frame of the run (ADR-7387): the first
/// reads the time before frame 1, the second reads frame 1, the start-up frame.
const START_UP_TICKS: u64 = 2;

/// Screenshot times, as fractions of the run (so a short run still gets the set).
pub const SHOT_FRACTIONS: [f32; 4] = [0.03, 0.30, 0.60, 0.95];

/// One reading of the machine's sensors; every field is optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SensorReading {
    /// Temperatures, °C, keyed `<hwmon name>/<label or tempN>` (APU `k10temp`, GPU `amdgpu`, …).
    pub temps_c: BTreeMap<String, f32>,
    /// Package power the GPU driver reports (`power1_average`), W.
    pub gpu_power_w: Option<f32>,
    /// GPU shader clock (`freq1_input`), MHz.
    pub gpu_mhz: Option<f32>,
    /// GPU busy share (`gpu_busy_percent`), %.
    pub gpu_busy_pct: Option<f32>,
    /// CPU core 0 clock (`scaling_cur_freq`), MHz.
    pub cpu_mhz: Option<f32>,
    /// Battery charge, %.
    pub battery_pct: Option<f32>,
    /// Battery discharge (or charge) power, W.
    pub battery_w: Option<f32>,
    /// Battery state (`Discharging`, `Charging`, `Full`).
    pub battery_status: Option<String>,
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn read_f32(path: &Path) -> Option<f32> {
    read_trim(path)?.parse::<f32>().ok()
}

fn dirs_sorted(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
        .collect();
    v.sort();
    v
}

/// Reads the sensors below `root` (`/sys` on a real machine).
pub fn read_sensors(root: &Path) -> SensorReading {
    let mut out = SensorReading::default();
    for hwmon in dirs_sorted(&root.join("class/hwmon"), "hwmon") {
        let name = read_trim(&hwmon.join("name")).unwrap_or_else(|| "hwmon".into());
        for temp in dirs_sorted(&hwmon, "temp") {
            let file = temp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Some(stem) = file.strip_suffix("_input") else {
                continue;
            };
            if let Some(milli) = read_f32(&temp) {
                let label =
                    read_trim(&hwmon.join(format!("{stem}_label"))).unwrap_or_else(|| stem.to_owned());
                out.temps_c.insert(format!("{name}/{label}"), milli / 1000.0);
            }
        }
        if name == "amdgpu" {
            // power1_average is microwatts, freq1_input hertz.
            out.gpu_power_w = read_f32(&hwmon.join("power1_average")).map(|uw| uw / 1.0e6);
            out.gpu_mhz = read_f32(&hwmon.join("freq1_input")).map(|hz| hz / 1.0e6);
        }
    }
    for card in dirs_sorted(&root.join("class/drm"), "card") {
        if let Some(busy) = read_f32(&card.join("device/gpu_busy_percent")) {
            out.gpu_busy_pct = Some(busy);
            break;
        }
    }
    out.cpu_mhz =
        read_f32(&root.join("devices/system/cpu/cpu0/cpufreq/scaling_cur_freq")).map(|khz| khz / 1000.0);
    for supply in dirs_sorted(&root.join("class/power_supply"), "") {
        if read_trim(&supply.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        out.battery_pct = read_f32(&supply.join("capacity"));
        out.battery_w = read_f32(&supply.join("power_now")).map(|uw| uw / 1.0e6);
        out.battery_status = read_trim(&supply.join("status"));
        break;
    }
    out
}

impl SensorReading {
    fn to_json(&self, t_s: f32) -> Value {
        json!({
            "t_s": round1(t_s),
            "temps_c": self.temps_c,
            "gpu_power_w": self.gpu_power_w,
            "gpu_mhz": self.gpu_mhz,
            "gpu_busy_pct": self.gpu_busy_pct,
            "cpu_mhz": self.cpu_mhz,
            "battery_pct": self.battery_pct,
            "battery_w": self.battery_w,
            "battery_status": self.battery_status,
        })
    }
}

fn round1(v: f32) -> f64 {
    (f64::from(v) * 10.0).round() / 10.0
}

fn round2(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

/// What the report records about the machine: OS release, kernel, the Steam/gamescope
/// environment (is this Game Mode?), CPU model.
pub fn system_info(root: &Path, env: impl Fn(&str) -> Option<String>) -> Value {
    let os_release = std::fs::read_to_string(root.join("etc/os-release")).unwrap_or_default();
    let field = |key: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(|v| v.trim_matches('"').to_owned())
    };
    let cpu = std::fs::read_to_string(root.join("proc/cpuinfo"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("model name")
                .and_then(|r| r.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        });
    json!({
        "os": field("PRETTY_NAME"),
        "os_build": field("BUILD_ID"),
        "kernel": read_trim(&root.join("proc/sys/kernel/osrelease")),
        "cpu": cpu,
        "steam_deck_env": env("SteamDeck"),
        "game_mode": env("GAMESCOPE_WAYLAND_DISPLAY").is_some() || env("XDG_CURRENT_DESKTOP").as_deref() == Some("gamescope"),
        "session": env("XDG_SESSION_TYPE"),
        "gamescope_fps_limit": env("GAMESCOPE_FPS_LIMIT"),
    })
}

/// Frame-time statistics of a run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameStats {
    /// Frames measured.
    pub frames: usize,
    /// Mean frame time, ms.
    pub mean_ms: f32,
    /// Median frame time, ms.
    pub p50_ms: f32,
    /// 95th percentile, ms.
    pub p95_ms: f32,
    /// 99th percentile, ms.
    pub p99_ms: f32,
    /// Slowest frame, ms.
    pub max_ms: f32,
    /// Average fps (frames over the summed frame time).
    pub avg_fps: f32,
    /// Fps of the slowest 1 % of frames (`1000 / p99`).
    pub low_1pct_fps: f32,
    /// Share of frames slower than 1.25 × the target period, % (a missed deadline).
    pub missed_pct: f32,
    /// Share of frames over 50 ms (a visible hitch), %.
    pub over_50ms_pct: f32,
}

/// Percentile (nearest rank) of an ascending slice.
fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f32).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl FrameStats {
    /// Statistics of `frame_ms` against a `target_fps` cap (`None` = judged against 60 fps).
    pub fn of(frame_ms: &[f32], target_fps: Option<f32>) -> Self {
        if frame_ms.is_empty() {
            return Self::default();
        }
        let mut sorted = frame_ms.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let total: f32 = frame_ms.iter().sum();
        let period = 1000.0 / target_fps.filter(|f| *f > 0.0).unwrap_or(60.0);
        let share = |limit: f32| {
            100.0 * frame_ms.iter().filter(|m| **m > limit).count() as f32 / frame_ms.len() as f32
        };
        let p99 = percentile(&sorted, 99.0);
        Self {
            frames: frame_ms.len(),
            mean_ms: total / frame_ms.len() as f32,
            p50_ms: percentile(&sorted, 50.0),
            p95_ms: percentile(&sorted, 95.0),
            p99_ms: p99,
            max_ms: sorted[sorted.len() - 1],
            avg_fps: 1000.0 * frame_ms.len() as f32 / total.max(1e-3),
            low_1pct_fps: if p99 > 0.0 { 1000.0 / p99 } else { 0.0 },
            missed_pct: share(period * 1.25),
            over_50ms_pct: share(50.0),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "frames": self.frames,
            "mean_ms": round2(self.mean_ms),
            "p50_ms": round2(self.p50_ms),
            "p95_ms": round2(self.p95_ms),
            "p99_ms": round2(self.p99_ms),
            "max_ms": round2(self.max_ms),
            "avg_fps": round1(self.avg_fps),
            "low_1pct_fps": round1(self.low_1pct_fps),
            "missed_deadline_pct": round2(self.missed_pct),
            "over_50ms_pct": round2(self.over_50ms_pct),
        })
    }
}

/// Mean and maximum of a series, accumulated without storing it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Agg {
    sum: f64,
    max: f32,
    n: u32,
}

impl Agg {
    /// Adds one value.
    pub fn add(&mut self, v: f32) {
        self.sum += f64::from(v);
        self.max = self.max.max(v);
        self.n += 1;
    }

    /// The mean (0 when empty).
    pub fn mean(&self) -> f32 {
        if self.n == 0 {
            0.0
        } else {
            (self.sum / f64::from(self.n)) as f32
        }
    }
}

/// The run's settings.
#[derive(Debug, Clone)]
pub struct DeckCheck {
    /// How long to run.
    pub duration: Duration,
    /// Where `deck_check.json` and the screenshots go.
    pub out_dir: PathBuf,
    /// Frame cap, fps (`None` = uncapped, to measure headroom).
    pub cap_fps: Option<f32>,
    /// Sysfs root (`/sys`; tests point it at a fake tree).
    pub sysfs: PathBuf,
    /// The Deck performance preset whose knobs the run applies (`--preset`, ADR-1332); `None`
    /// runs the plain `Deck` render profile.
    pub preset: Option<regia_steam::DeckPerfPreset>,
    /// The panel the preset's knobs are chosen for (`--panel`; LCD by default).
    pub panel: regia_steam::DeckPanel,
}

impl DeckCheck {
    /// A run of `duration` writing to `out_dir`, capped at the Deck default of 40 fps.
    pub fn new(duration: Duration, out_dir: impl Into<PathBuf>) -> Self {
        Self {
            duration,
            out_dir: out_dir.into(),
            cap_fps: Some(40.0),
            sysfs: PathBuf::from("/sys"),
            preset: None,
            panel: regia_steam::DeckPanel::Lcd,
        }
    }

    /// The preset's knobs, when the run applies one.
    pub fn knobs(&self) -> Option<regia_steam::DeckPerfKnobs> {
        self.preset.map(|preset| preset.knobs(self.panel))
    }
}

/// Run state.
#[derive(Resource)]
struct State {
    cfg: DeckCheck,
    /// `deck_tick` calls so far. The deltas read by the first `START_UP_TICKS` calls are not
    /// frames of the run: the time before frame 1, then frame 1 itself, which carried the start-up
    /// (pipeline compiles) (ADR-7387).
    ticks: u64,
    /// The run so far on the app's clock (ADR-9600): the sum of the frames' unclamped
    /// [`RealTime::raw_delta`] after the start-up ticks. The wall clock under the default time
    /// strategy; exact under `TimeUpdateStrategy::ManualDuration`.
    run: Duration,
    frame_ms: Vec<f32>,
    gpu_frame: Agg,
    passes: BTreeMap<&'static str, Agg>,
    gpu_timing_on: bool,
    sensors: Vec<Value>,
    peak_temp_c: f32,
    next_sensor: Duration,
    shots_due: Vec<(Duration, String)>,
    shots_done: Arc<Mutex<Vec<String>>>,
    adapter: Option<String>,
    /// The panel the adapter's PCI device id names (`DeckPanel::from_gpu_device_id`).
    panel_detected: Option<regia_steam::DeckPanel>,
    window: Option<(u32, u32)>,
    /// Frames that ran while the runner had the window hidden or stalled (no frame callbacks:
    /// a blanked panel or a covered window). Those frames are throttled and not presented.
    hidden_frames: u32,
    finished: bool,
}

impl State {
    fn new(cfg: DeckCheck, shots_due: Vec<(Duration, String)>) -> Self {
        Self {
            ticks: 0,
            run: Duration::ZERO,
            frame_ms: Vec::with_capacity(60 * 330),
            gpu_frame: Agg::default(),
            passes: BTreeMap::new(),
            gpu_timing_on: false,
            sensors: Vec::new(),
            peak_temp_c: 0.0,
            next_sensor: Duration::ZERO,
            shots_due,
            shots_done: Arc::default(),
            adapter: None,
            panel_detected: None,
            window: None,
            hidden_frames: 0,
            finished: false,
            cfg,
        }
    }
}

/// A moving unit (box) on a circle.
#[derive(Component)]
struct Unit {
    radius: f32,
    angular_speed: f32,
    phase: f32,
}

/// The plugin; add it after [`crate::window::WindowedVfxLab`].
pub struct DeckCheckPlugin(pub DeckCheck);

impl Plugin for DeckCheckPlugin {
    fn build(&self, app: &mut App) {
        let cfg = self.0.clone();
        let shots_due = SHOT_FRACTIONS
            .iter()
            .enumerate()
            .map(|(i, f)| {
                (
                    cfg.duration.mul_f32(*f),
                    format!("deck_{i}_{:03}pct.png", (f * 100.0) as u32),
                )
            })
            .collect();
        let mut settings =
            regia_render::RenderSettings::from_profile(regia_render::RenderProfilePreset::Deck);
        if let Some(knobs) = cfg.knobs() {
            crate::deck_presets::apply_knobs(&mut settings.profile, &knobs);
            // An explicit `--cap` beats the preset's own cap in the profile too.
            settings.profile.frame_cap_fps = cfg.cap_fps.map(|fps| fps.round() as u32);
        }
        if cfg.cap_fps.is_none() {
            // Uncapped means uncapped: the Deck profile's own 40 fps cap would otherwise be
            // re-applied to `FramePacing` and the headroom number would read the cap.
            settings.profile.frame_cap_fps = None;
        }
        app.insert_resource(settings);
        app.world_mut().resource_mut::<FramePacing>().target_fps = cfg.cap_fps;
        app.insert_resource(State::new(cfg, shots_due))
            .add_systems(Startup, spawn_units)
            .add_systems(PreUpdate, (scripted_camera, move_units))
            .add_systems(Last, deck_tick);
    }
}

/// 48 boxes on rings around the origin, so the CPU transform/extract path has real work.
fn spawn_units(world: &mut World) {
    let mesh = Arc::new(CpuMesh {
        vertices: crate::window::box_vertices(0.8, 0.5, 1.4),
        indices: crate::window::box_indices(),
    });
    for i in 0..48u32 {
        let ring = 1 + i / 16;
        let radius = 12.0 + 9.0 * ring as f32;
        let material = Arc::new(CpuMaterial {
            base_color: [0.3 + 0.1 * (i % 3) as f32, 0.3, 0.25 + 0.1 * (i % 4) as f32, 1.0],
            roughness: 0.7,
            ..Default::default()
        });
        let transform = Transform::from_translation(Vec3::new(radius, 0.25, 0.0));
        world.spawn((
            Unit {
                radius,
                angular_speed: 0.25 / ring as f32,
                phase: (i % 16) as f32 / 16.0 * std::f32::consts::TAU,
            },
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer {
                mesh: Arc::clone(&mesh),
                material,
                layer: 0,
                decal_layer: 0,
                gi_proxy: true,
            },
        ));
    }
}

fn move_units(time: Res<RealTime>, mut units: Query<(&Unit, &mut Transform)>) {
    let t = time.elapsed().as_secs_f32();
    for (u, mut tf) in &mut units {
        let a = u.phase + t * u.angular_speed;
        tf.translation = Vec3::new(u.radius * a.cos(), 0.25, u.radius * a.sin());
        tf.rotation = glam::Quat::from_rotation_y(-a);
    }
}

/// The camera path: a slow orbit that breathes between close (effects) and wide (units, sky).
fn scripted_camera(time: Res<RealTime>, orbit: Option<ResMut<crate::window::OrbitState>>) {
    let Some(mut orbit) = orbit else {
        return;
    };
    let t = time.elapsed().as_secs_f32();
    orbit.yaw = 0.65 + t * 0.06;
    orbit.pitch = 0.12 + 0.12 * (t * 0.05).sin().abs();
    orbit.distance = 11.0 + 9.0 * (t * 0.04).sin().abs();
}

#[allow(clippy::too_many_arguments)]
fn deck_tick(world: &mut World) {
    let _span = tracing::trace_span!("deck_check").entered();
    let Some(mut state) = world.remove_resource::<State>() else {
        return;
    };
    if state.finished {
        world.insert_resource(state);
        return;
    }
    // ADR-9600: the run is timed on the app's clock and starts at the first frame. Timed from the
    // plugin's build by the wall clock, an 8 s start-up on a loaded card used up a 4 s run before
    // its first frame was recorded (`frames: 0`).
    // ADR-7387: `raw_delta` at frame N is the length of frame N-1, so frame 1 reads the time before
    // it and frame 2 reads frame 1, the start-up frame (pipeline compiles: 0.9 s warm and 3.9 s
    // cold on the Deck). The run's first frame is frame 2, read at frame 3.
    let delta = world
        .get_resource::<RealTime>()
        .map_or(Duration::ZERO, RealTime::raw_delta);
    if state.ticks >= START_UP_TICKS {
        state.frame_ms.push((delta.as_secs_f64() * 1000.0) as f32);
        state.run = state.run.saturating_add(delta);
    }
    state.ticks += 1;
    let elapsed = state.run;
    if world
        .get_resource::<regia::platform::SurfacesHidden>()
        .is_some_and(|h| h.0)
    {
        state.hidden_frames += 1;
    }

    // GPU timestamps: switch on once, read the last frame's pass table every frame.
    if let Some(device) = world.get_resource::<regia_render::RenderDevice>().cloned() {
        if state.adapter.is_none() {
            let info = device.adapter.get_info();
            state.panel_detected = regia_steam::DeckPanel::from_gpu_device_id(info.device);
            state.adapter = Some(info.name);
        }
        if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
            if !state.gpu_timing_on && device.capabilities.timestamp_query {
                renderer.0.set_gpu_timing(&device, true);
                state.gpu_timing_on = true;
            }
            if state.gpu_timing_on && renderer.0.gpu_timing_enabled() {
                let t = renderer.0.gpu_timings();
                if t.frame_ms > 0.0 {
                    state.gpu_frame.add(t.frame_ms);
                    for (name, ms) in t.passes() {
                        state.passes.entry(name).or_default().add(ms);
                    }
                }
            }
        }
    }
    if state.window.is_none()
        && let Some(s) = world.get_resource::<regia_render::WindowSurfaces>()
    {
        state.window = s.primary_size();
    }

    if elapsed >= state.next_sensor {
        state.next_sensor = elapsed + SENSOR_PERIOD;
        let reading = read_sensors(&state.cfg.sysfs);
        for v in reading.temps_c.values() {
            state.peak_temp_c = state.peak_temp_c.max(*v);
        }
        state.sensors.push(reading.to_json(elapsed.as_secs_f32()));
    }

    let due: Vec<String> = state
        .shots_due
        .iter()
        .filter(|(at, _)| elapsed >= *at)
        .map(|(_, n)| n.clone())
        .collect();
    state.shots_due.retain(|(at, _)| elapsed < *at);
    for name in due {
        let path = state.cfg.out_dir.join(&name);
        let done = Arc::clone(&state.shots_done);
        world.resource_mut::<ScreenshotRequests>().request(
            None,
            Box::new(move |result| match result {
                Ok(image) => match path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| crate::save_png(&path, &image.rgba, image.width, image.height))
                {
                    Ok(()) => {
                        if let Ok(mut d) = done.lock() {
                            d.push(name);
                        }
                    }
                    Err(err) => tracing::error!("deck_check: screenshot save failed: {err}"),
                },
                Err(err) => tracing::error!("deck_check: screenshot failed: {err}"),
            }),
        );
    }

    if elapsed >= state.cfg.duration && state.shots_due.is_empty() {
        state.finished = true;
        let report = report_json(&state, elapsed);
        if let Err(err) = std::fs::create_dir_all(&state.cfg.out_dir).and_then(|()| {
            std::fs::write(
                state.cfg.out_dir.join("deck_check.json"),
                serde_json::to_string_pretty(&report).unwrap_or_default(),
            )
        }) {
            tracing::error!("deck_check: cannot write the report: {err}");
        }
        // The engine's summary.json carries the frame record; the short form goes in its host section.
        if let Some(mut host) = world.get_resource_mut::<regia::telemetry::TelemetryHost>() {
            host.set_value("app", "vfx_lab");
            host.set_value("build", "deck_check");
            host.set_value("deck_check", report["frames"].clone());
        }
        tracing::info!(
            "deck_check: done after {:.0} s, report in {}",
            elapsed.as_secs_f32(),
            state.cfg.out_dir.display()
        );
        world.write_message(AppExit::Success);
    }
    world.insert_resource(state);
}

fn report_json(state: &State, elapsed: Duration) -> Value {
    let stats = FrameStats::of(&state.frame_ms, state.cfg.cap_fps);
    let passes: BTreeMap<&str, Value> = state
        .passes
        .iter()
        .map(|(n, a)| (*n, json!({"mean_ms": round2(a.mean()), "max_ms": round2(a.max)})))
        .collect();
    let shots = state.shots_done.lock().map(|d| d.clone()).unwrap_or_default();
    json!({
        "schema": 1,
        "tool": "vfx_lab --deck-check",
        "profile": "Deck",
        // ADR-1332: the preset the run applied (null = the plain Deck profile), the panel its knobs
        // were chosen for, and the panel the GPU says this Deck has.
        "preset": state.cfg.preset.map(|p| p.name()),
        "panel": crate::deck_presets::panel_name(state.cfg.panel),
        "panel_detected": state.panel_detected.map(crate::deck_presets::panel_name),
        "knobs": state.cfg.knobs().map(|k| crate::deck_presets::knobs_json(&k)),
        "duration_s": round1(elapsed.as_secs_f32()),
        "window": state.window,
        "adapter": state.adapter,
        "system": system_info(Path::new("/"), |k| std::env::var(k).ok()),
        "cap_fps": state.cfg.cap_fps,
        // Non-zero means the compositor sent no frame callbacks (panel blanked, window covered)
        // and the frame times above are the throttled idle rate, not the engine's.
        "hidden_frames": state.hidden_frames,
        "frames": stats.to_json(),
        "gpu": {
            "timestamps": state.gpu_timing_on,
            "frame_mean_ms": round2(state.gpu_frame.mean()),
            "frame_max_ms": round2(state.gpu_frame.max),
            "passes": passes,
        },
        "sensors": {
            "period_s": SENSOR_PERIOD.as_secs(),
            "peak_temp_c": round1(state.peak_temp_c),
            "samples": state.sensors,
        },
        "screenshots": shots,
        "note": "quit timings are in summary.json (exit.ms and the per-step times)",
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn reads_a_deck_shaped_sysfs_tree() {
        let dir = std::env::temp_dir().join(format!("deck-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "class/hwmon/hwmon3/name", "amdgpu\n");
        write(&dir, "class/hwmon/hwmon3/temp1_input", "61000\n");
        write(&dir, "class/hwmon/hwmon3/temp1_label", "edge\n");
        write(&dir, "class/hwmon/hwmon3/power1_average", "9500000\n");
        write(&dir, "class/hwmon/hwmon3/freq1_input", "1600000000\n");
        write(&dir, "class/hwmon/hwmon5/name", "k10temp\n");
        write(&dir, "class/hwmon/hwmon5/temp1_input", "72500\n");
        write(&dir, "class/drm/card0/device/gpu_busy_percent", "97\n");
        write(
            &dir,
            "devices/system/cpu/cpu0/cpufreq/scaling_cur_freq",
            "3200000\n",
        );
        write(&dir, "class/power_supply/BAT1/type", "Battery\n");
        write(&dir, "class/power_supply/BAT1/capacity", "81\n");
        write(&dir, "class/power_supply/BAT1/power_now", "14200000\n");
        write(&dir, "class/power_supply/BAT1/status", "Discharging\n");
        write(&dir, "class/power_supply/ACAD/type", "Mains\n");
        let r = read_sensors(&dir);
        assert_eq!(r.temps_c.get("amdgpu/edge"), Some(&61.0));
        assert_eq!(r.temps_c.get("k10temp/temp1"), Some(&72.5));
        assert_eq!(r.gpu_power_w, Some(9.5));
        assert_eq!(r.gpu_mhz, Some(1600.0));
        assert_eq!(r.gpu_busy_pct, Some(97.0));
        assert_eq!(r.cpu_mhz, Some(3200.0));
        assert_eq!((r.battery_pct, r.battery_w), (Some(81.0), Some(14.2)));
        assert_eq!(r.battery_status.as_deref(), Some("Discharging"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_info_reads_a_steamos_tree() {
        let dir = std::env::temp_dir().join(format!("deck-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(
            &dir,
            "etc/os-release",
            "NAME=\"SteamOS\"\nPRETTY_NAME=\"SteamOS Holo\"\nBUILD_ID=20260915.1\n",
        );
        write(&dir, "proc/sys/kernel/osrelease", "6.5.0-valve22-1-neptune\n");
        write(
            &dir,
            "proc/cpuinfo",
            "processor\t: 0\nmodel name\t: AMD Custom APU 0405\n",
        );
        let info = system_info(&dir, |k| match k {
            "SteamDeck" => Some("1".into()),
            "GAMESCOPE_WAYLAND_DISPLAY" => Some("gamescope-0".into()),
            _ => None,
        });
        assert_eq!(info["os"], "SteamOS Holo");
        assert_eq!(info["os_build"], "20260915.1");
        assert_eq!(info["kernel"], "6.5.0-valve22-1-neptune");
        assert_eq!(info["cpu"], "AMD Custom APU 0405");
        assert_eq!(info["steam_deck_env"], "1");
        assert_eq!(info["game_mode"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_sysfs_is_an_empty_reading() {
        let r = read_sensors(Path::new("/definitely/not/here"));
        assert_eq!(r, SensorReading::default());
    }

    #[test]
    fn frame_stats_percentiles_and_deadlines() {
        // 100 frames: 90 at 25 ms (40 fps), 8 at 30 ms, 2 at 60 ms.
        let mut ms = vec![25.0; 90];
        ms.extend([30.0; 8]);
        ms.extend([60.0; 2]);
        let s = FrameStats::of(&ms, Some(40.0));
        assert_eq!(s.frames, 100);
        assert_eq!(s.p50_ms, 25.0);
        assert_eq!(s.p95_ms, 30.0);
        assert_eq!(s.p99_ms, 60.0);
        assert_eq!(s.max_ms, 60.0);
        assert!((s.avg_fps - 1000.0 * 100.0 / 2610.0).abs() < 0.01);
        // Deadline 1.25 x 25 = 31.25 ms: only the two 60 ms frames missed.
        assert!((s.missed_pct - 2.0).abs() < 1e-4);
        assert!((s.over_50ms_pct - 2.0).abs() < 1e-4);
        assert_eq!(FrameStats::of(&[], None), FrameStats::default());
    }

    /// ADR-7387, on the wall clock the Deck runs on (`RealTime::tick`): the first frame reads a
    /// zero delta, and the second reads the first frame's length. A 900 ms start-up frame (warm
    /// Mesa cache on the Deck; 3.9 s cold) counted as a frame of the run made the 1-minute 30 fps
    /// run report 29.5 fps and a 940 ms max.
    #[test]
    fn the_start_up_frame_is_not_a_frame_of_the_run_on_the_wall_clock() {
        let dir = regia_test_utils::unique_temp_dir("deck-check-wall-clock");
        let mut cfg = DeckCheck::new(Duration::from_millis(100), dir.join("out"));
        cfg.sysfs = dir.join("no-sysfs");
        let mut app = App::new();
        app.init_resource::<ScreenshotRequests>();
        app.insert_resource(State::new(cfg, Vec::new()));
        let t0 = Instant::now();
        // Frame starts: t0, then the start-up frame lasts 900 ms, then 25 ms frames.
        let starts = std::iter::once(t0)
            .chain((0..19u32).map(|i| t0 + Duration::from_millis(900) + Duration::from_millis(25) * i));
        let mut ticks = 0;
        for start in starts {
            app.world_mut().resource_mut::<RealTime>().tick(start);
            deck_tick(app.world_mut());
            ticks += 1;
            if app.world().resource::<State>().finished {
                break;
            }
        }
        let state = app.world().resource::<State>();
        assert!(state.finished, "the 100 ms run ends within 20 frames");
        assert_eq!(ticks, 6);
        assert_eq!(state.frame_ms, vec![25.0; 4]);
        let text = std::fs::read_to_string(dir.join("out/deck_check.json")).unwrap();
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["frames"]["frames"], 4, "{text}");
        assert_eq!(report["frames"]["max_ms"], 25.0, "{text}");
        assert_eq!(report["frames"]["avg_fps"], 40.0, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-9600: the run is timed on the app's clock (`RealTime`, so a `ManualDuration` run is
    /// exact) and starts at the first frame, so a long start-up (pipeline compiles: 8 s on a loaded
    /// 5090) cannot use up the run before any frame is recorded.
    #[test]
    fn a_slow_first_frame_does_not_eat_the_run() {
        let dir = regia_test_utils::unique_temp_dir("deck-check-clock");
        let mut cfg = DeckCheck::new(Duration::from_millis(100), dir.join("out"));
        cfg.sysfs = dir.join("no-sysfs");
        let mut app = App::new();
        app.init_resource::<ScreenshotRequests>();
        app.insert_resource(State::new(cfg, Vec::new()));
        // As the app clock reports it (ADR-7387): `RealTime::raw_delta` at frame N is the length
        // of frame N-1, so frame 1 reads the time before it (zero on the wall clock; 10 s here, a
        // start-up before the first frame) and frame 2 reads frame 1 itself, the start-up frame
        // that carried the pipeline compiles (10 s). Then 25 ms frames.
        let deltas = [Duration::from_secs(10), Duration::from_secs(10)]
            .into_iter()
            .chain(std::iter::repeat(Duration::from_millis(25)))
            .take(20);
        let mut ticks = 0;
        for delta in deltas {
            app.world_mut().resource_mut::<RealTime>().advance_by(delta);
            deck_tick(app.world_mut());
            ticks += 1;
            if app.world().resource::<State>().finished {
                break;
            }
        }
        let state = app.world().resource::<State>();
        assert!(state.finished, "the 100 ms run ends within 20 frames");
        // Neither the time before frame 1 nor frame 1 (the start-up frame) is a frame of the run;
        // four 25 ms frames fill it.
        assert_eq!(ticks, 6);
        assert_eq!(state.frame_ms, vec![25.0; 4]);
        let text = std::fs::read_to_string(dir.join("out/deck_check.json")).unwrap();
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["frames"]["frames"], 4, "{text}");
        assert_eq!(report["duration_s"], 0.1, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agg_tracks_mean_and_max() {
        let mut a = Agg::default();
        for v in [1.0, 2.0, 6.0] {
            a.add(v);
        }
        assert_eq!((a.mean(), a.max), (3.0, 6.0));
        assert_eq!(Agg::default().mean(), 0.0);
    }
}
