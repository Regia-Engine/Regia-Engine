// SPDX-License-Identifier: MIT-0
//! The benchmark flythrough (ADR-3013): the panel's button, F9 or `demo.benchmark` plays every view of the
//! scene's `perf_views.json` at the tier the demo runs and writes `showcase-table.md` and `showcase-table.json`
//! with [`crate::perf_table`], the writer of `cargo xtask showcase-table`.
//!
//! Each view's camera moves one 60 Hz step per frame ([`VIEW_FPS`], as in the xtask's render-out run), whatever
//! the frame rate, so every run of a view shows the same pictures; the measured value is the wall time between
//! two presented frames ([`super::fps::FrameClock`]). The first `warmup` frames of a view are drawn and not
//! measured. The flight input is ignored while it runs, and the camera returns to where it was afterwards. A
//! cancelled run writes its table too, with the views it did not finish `not run`.

use std::path::PathBuf;

use regia::prelude::*;
use regia_framework::shots::{CameraPath, Shot, ShotCamera, ShotSequence, Track};
use regia_render::{AdapterProfile, Camera, QualityTier, RenderDevice};
use regia_scene::Transform;
use serde_json::{Value, json};

use super::fly::DemoCamera;
use super::fps::FpsMeter;
use super::{DemoError, DemoErrorCode, DemoSettings, DemoState};
use crate::perf_table::{
    Machine, Row, Stamp, TIERS, TableSource, Tier, VIEW_FPS, View, check_view_length, is_software_adapter,
    json, markdown, not_run, parse_views, view_stats,
};

/// Warm-up frames per view by default (as the xtask).
pub const DEFAULT_WARMUP: u32 = 30;
/// Measured frames per view by default (as the xtask).
pub const DEFAULT_FRAMES: u32 = 240;
/// Most frames (warm-up or measured) one view may take.
pub const MAX_FRAMES: u32 = 3600;
/// The views file the benchmark plays, in the scene directory.
pub const VIEWS_FILE: &str = "perf_views.json";

/// The table row of the Medium tier (Desktop High drawn at 67 %), which the xtask's ladder does not run.
pub const MEDIUM_TIER: Tier = Tier {
    name: "medium",
    preset: "DesktopHigh",
    forward_lite: None,
    needs: "As high, drawn at 67 % render scale with temporal upscaling (the demo's Medium tier)",
};

/// The table row of the Very Low tier (Desktop Low drawn at 33 % with SGSR, ADR-7882), which the xtask's ladder
/// does not run.
pub const VERY_LOW_TIER: Tier = Tier {
    name: "very-low",
    preset: "DesktopLow",
    forward_lite: None,
    needs: "As low, drawn at 33 % render scale with SGSR upscaling (the step below Low for the slowest built-in graphics)",
};

/// The table row of a quality tier.
#[must_use]
pub fn table_tier(tier: QualityTier) -> Tier {
    let name = match tier {
        QualityTier::Ultra => "ultra",
        QualityTier::High => "high",
        QualityTier::Low => "low",
        QualityTier::VeryLow => return VERY_LOW_TIER,
        QualityTier::Deck => "deck",
        QualityTier::MobileHigh => "mobile-high",
        QualityTier::MobileLow => "mobile-low",
        QualityTier::Medium => return MEDIUM_TIER,
    };
    TIERS
        .iter()
        .copied()
        .find(|t| t.name == name)
        .unwrap_or(MEDIUM_TIER)
}

/// One view ready to play: its table entry and its one-shot camera sequence.
#[derive(Clone, Debug)]
pub struct BenchView {
    /// The table's view (name, `camera.set_path` params, length).
    pub view: View,
    /// The camera path as a one-shot sequence.
    pub sequence: ShotSequence,
}

/// A view's params as a one-shot sequence (the path, and the fov and roll tracks when given).
///
/// # Errors
/// A message when the path or a track does not parse or the sequence does not validate.
pub fn view_sequence(view: &View) -> Result<ShotSequence, String> {
    let field = |key: &str| view.params.get(key).filter(|v| !v.is_null()).cloned();
    let path: CameraPath = serde_json::from_value(field("path").unwrap_or(Value::Null))
        .map_err(|e| format!("view `{}`: path: {e}", view.name))?;
    let track = |key: &str| -> Result<Track, String> {
        field(key).map_or(Ok(Track::default()), |v| {
            serde_json::from_value(v).map_err(|e| format!("view `{}`: {key}: {e}", view.name))
        })
    };
    let sequence = ShotSequence {
        name: view.name.clone(),
        shots: vec![Shot {
            name: view.name.clone(),
            duration: view.seconds as f32,
            camera: ShotCamera {
                path,
                fov: track("fov")?,
                roll: track("roll")?,
                dof: None,
                rotation: Vec::new(),
            },
            fade_in: 0.0,
            fade_out: 0.0,
            fade_color: [0.0; 3],
        }],
        ..ShotSequence::default()
    };
    sequence
        .validate()
        .map_err(|e| format!("view `{}`: {e}", view.name))?;
    Ok(sequence)
}

/// A running benchmark.
#[derive(Clone, Debug)]
pub struct BenchRun {
    /// The views, in file order.
    pub views: Vec<BenchView>,
    /// The view playing.
    pub view: usize,
    /// The frame of that view (warm-up included).
    pub frame: u32,
    /// Warm-up frames per view.
    pub warmup: u32,
    /// Measured frames per view.
    pub frames: u32,
    /// The playing view's measured frame times, milliseconds.
    pub samples: Vec<f64>,
    /// Finished views.
    pub rows: Vec<Row>,
    /// The tier's table row.
    pub tier: Tier,
    /// Where the table goes.
    pub out: PathBuf,
    /// The camera's pose and field of view before the run (restored after it).
    pub saved: Option<(Entity, Transform, f32)>,
}

/// A finished benchmark.
#[derive(Clone, Debug, PartialEq)]
pub struct BenchResult {
    /// The tier's table name.
    pub tier: String,
    /// The Markdown table.
    pub markdown: PathBuf,
    /// The JSON record.
    pub json: PathBuf,
    /// The rows.
    pub rows: Vec<Row>,
    /// Stopped before the last view.
    pub cancelled: bool,
    /// The table could not be written (the message).
    pub error: Option<String>,
}

/// The benchmark's state in [`DemoState`].
#[derive(Clone, Debug, Default)]
pub struct BenchStatus {
    /// The running benchmark.
    pub run: Option<BenchRun>,
    /// The last finished one.
    pub last: Option<BenchResult>,
    /// Benchmarks finished (cancelled ones included).
    pub runs: u32,
}

impl BenchStatus {
    /// A benchmark is running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// One line for the panel.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(run) = &self.run {
            let name = run.views.get(run.view).map_or("", |v| v.view.name.as_str());
            return format!(
                "Benchmark: view {}/{} ({name}), frame {}/{} (Esc stops)",
                run.view + 1,
                run.views.len(),
                run.frame,
                run.warmup + run.frames
            );
        }
        match &self.last {
            Some(last) if last.error.is_some() => {
                format!(
                    "Benchmark table not written: {}",
                    last.error.as_deref().unwrap_or_default()
                )
            }
            Some(last) => {
                let avg: Vec<String> = last
                    .rows
                    .iter()
                    .map(|r| match &r.stats {
                        Some(s) => format!("{} {} fps", r.view, super::fps::fps_text(s.avg_fps)),
                        None => format!("{} not run", r.view),
                    })
                    .collect();
                format!(
                    "Benchmark {}{}: {} ({})",
                    last.tier,
                    if last.cancelled { ", stopped" } else { "" },
                    avg.join(", "),
                    last.markdown.display()
                )
            }
            None => "Benchmark: not run yet (F9)".to_owned(),
        }
    }

    /// The status as JSON (`demo.state`'s `benchmark`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let run = self.run.as_ref().map(|run| {
            json!({
                "view": run.view,
                "view_name": run.views.get(run.view).map(|v| v.view.name.clone()),
                "views": run.views.iter().map(|v| v.view.name.clone()).collect::<Vec<_>>(),
                "frame": run.frame,
                "warmup": run.warmup,
                "frames": run.frames,
                "tier": run.tier.name,
                "out": run.out.display().to_string(),
            })
        });
        let last = self.last.as_ref().map(|last| {
            json!({
                "tier": last.tier,
                "markdown": last.markdown.display().to_string(),
                "json": last.json.display().to_string(),
                "cancelled": last.cancelled,
                "error": last.error,
                "rows": last.rows.iter().map(|r| json!({
                    "view": r.view,
                    "status": if r.stats.is_some() { "measured" } else { "not run" },
                    "frames": r.stats.as_ref().map(|s| s.frames),
                    "avg_fps": r.stats.as_ref().map(|s| s.avg_fps),
                    "min_fps": r.stats.as_ref().map(|s| s.min_fps),
                    "p99_ms": r.stats.as_ref().map(|s| s.p99_ms),
                    "note": r.note,
                })).collect::<Vec<_>>(),
            })
        });
        json!({ "running": self.running(), "run": run, "last": last, "runs": self.runs, "text": self.summary() })
    }
}

/// Starts the benchmark over the scene's views.
///
/// # Errors
/// [`DemoErrorCode::BenchRunning`], [`DemoErrorCode::BadValue`] (frames out of range, a bad `out`),
/// [`DemoErrorCode::NoViews`] or [`DemoErrorCode::ViewsMalformed`].
pub fn start(
    world: &mut World,
    warmup: Option<u32>,
    frames: Option<u32>,
    out: Option<PathBuf>,
) -> Result<Value, DemoError> {
    if world.resource::<DemoState>().bench.running() {
        return Err(DemoError::new(
            DemoErrorCode::BenchRunning,
            "a benchmark is running; cancel it first",
        ));
    }
    let settings = world.resource::<DemoSettings>().0.clone();
    let warmup = warmup.unwrap_or(settings.bench_warmup);
    let frames = frames.unwrap_or(settings.bench_frames);
    if warmup > MAX_FRAMES || !(1..=MAX_FRAMES).contains(&frames) {
        return Err(DemoError::new(
            DemoErrorCode::BadValue,
            format!("warmup must be within 0..={MAX_FRAMES} and frames within 1..={MAX_FRAMES}"),
        ));
    }
    let base = settings.out_dir.join("benchmark");
    let out = match out {
        Some(name) => super::capture::contained_path(&base, &name, None)?,
        None => base,
    };
    let path = super::scene_dir(world).join(VIEWS_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        DemoError::new(
            DemoErrorCode::NoViews,
            format!(
                "{}: {e} (the benchmark plays the scene's {VIEWS_FILE})",
                path.display()
            ),
        )
    })?;
    let malformed =
        |e: String| DemoError::new(DemoErrorCode::ViewsMalformed, format!("{}: {e}", path.display()));
    let views = parse_views(&text).map_err(malformed)?;
    let mut bench_views = Vec::with_capacity(views.len());
    for view in views {
        check_view_length(&view, warmup, frames).map_err(malformed)?;
        let sequence = view_sequence(&view).map_err(malformed)?;
        bench_views.push(BenchView { view, sequence });
    }
    let saved = camera_pose(world);
    let tier = table_tier(world.resource::<DemoState>().tier);
    let names: Vec<String> = bench_views.iter().map(|v| v.view.name.clone()).collect();
    let answer = json!({
        "started": true,
        "views": names,
        "warmup": warmup,
        "frames": frames,
        "tier": tier.name,
        "out": out.display().to_string(),
    });
    world.resource_mut::<DemoState>().bench.run = Some(BenchRun {
        views: bench_views,
        view: 0,
        frame: 0,
        warmup,
        frames,
        samples: Vec::new(),
        rows: Vec::new(),
        tier,
        out,
        saved,
    });
    Ok(answer)
}

/// Stops the running benchmark and writes its table (unfinished views `not run`).
///
/// # Errors
/// [`DemoErrorCode::BenchIdle`] when none runs; [`DemoErrorCode::BenchWriteFailed`] when the table could not be
/// written.
pub fn cancel(world: &mut World) -> Result<Value, DemoError> {
    let Some(mut run) = world.resource_mut::<DemoState>().bench.run.take() else {
        return Err(DemoError::new(
            DemoErrorCode::BenchIdle,
            "no benchmark is running",
        ));
    };
    for view in run.views.iter().skip(run.rows.len()) {
        run.rows.push(not_run(&run.tier, &view.view, "cancelled"));
    }
    finish(world, run, true)
}

fn camera_pose(world: &mut World) -> Option<(Entity, Transform, f32)> {
    let mut demo = world.query_filtered::<(Entity, &Transform, &Camera), With<DemoCamera>>();
    if let Some((e, t, c)) = demo.iter(world).next() {
        return Some((e, *t, c.fov_y));
    }
    let mut any = world.query::<(Entity, &Transform, &Camera)>();
    any.iter(world).next().map(|(e, t, c)| (e, *t, c.fov_y))
}

/// `PostUpdate`, after the camera rigs: puts the camera on the playing view's path for this frame.
pub fn drive(mut state: ResMut<DemoState>, mut cameras: Query<(&mut Transform, &mut Camera)>) {
    let Some(run) = state.bench.run.as_mut() else {
        return;
    };
    let _span = tracing::info_span!("demo_bench_drive").entered();
    let Some(view) = run.views.get(run.view) else {
        return;
    };
    let time = f64::from(run.frame) / f64::from(VIEW_FPS);
    let Some(sample) = view.sequence.sample(time) else {
        return;
    };
    let camera = match run.saved.map(|(e, _, _)| e) {
        Some(entity) => cameras.get_mut(entity).ok(),
        None => cameras.iter_mut().next(),
    };
    if let Some((mut transform, mut camera)) = camera {
        transform.translation = sample.eye;
        transform.rotation = sample.rotation;
        camera.fov_y = sample.fov_y_degrees.to_radians();
    }
}

/// `Last`, after the frame-rate meter: records the frame time and advances the run; writes the table after the
/// last view.
pub fn measure(world: &mut World) {
    let last_ms = world.resource::<FpsMeter>().last_ms;
    let finished = {
        let mut state = world.resource_mut::<DemoState>();
        let Some(run) = state.bench.run.as_mut() else {
            return;
        };
        if run.frame >= run.warmup
            && let Some(ms) = last_ms
        {
            run.samples.push(ms);
        }
        run.frame += 1;
        if run.frame >= run.warmup + run.frames {
            let samples = std::mem::take(&mut run.samples);
            if let Some(view) = run.views.get(run.view) {
                let row = match view_stats(&samples) {
                    Some(stats) => Row {
                        tier: run.tier.name.to_owned(),
                        view: view.view.name.clone(),
                        stats: Some(stats),
                        note: String::new(),
                    },
                    None => not_run(&run.tier, &view.view, "no usable frame times"),
                };
                run.rows.push(row);
            }
            run.view += 1;
            run.frame = 0;
        }
        run.view >= run.views.len()
    };
    if finished
        && let Some(run) = world.resource_mut::<DemoState>().bench.run.take()
        && let Err(error) = finish(world, run, false)
    {
        tracing::warn!(target: "showcase", "demo: {error}");
    }
}

/// Today's date (UTC) as `YYYY-MM-DD`, from days since 1970 (Hinnant's civil-from-days).
#[must_use]
pub fn date_from_unix_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    date_from_unix_days(i64::try_from(secs / 86_400).unwrap_or(0))
}

/// The machine the run measured, from the GPU device.
fn machine(world: &World, tier: &Tier) -> Machine {
    let Some(device) = world.get_resource::<RenderDevice>() else {
        return Machine {
            profile: tier.preset.to_owned(),
            ..Machine::default()
        };
    };
    let adapter = AdapterProfile::from_device(device);
    Machine {
        adapter: adapter.name.clone(),
        backend: adapter.backend.clone(),
        storage_buffers: Some(u64::from(adapter.max_storage_buffers_per_shader_stage)),
        multi_draw_indirect_count: None,
        timestamp_query: Some(adapter.timestamps),
        profile: tier.preset.to_owned(),
    }
}

/// The size frames were presented at: the window's, else the app's render size.
fn frame_size(world: &mut World) -> (u32, u32) {
    let mut windows = world.query::<&regia_platform::Window>();
    if let Some(window) = windows.iter(world).next() {
        return window.physical_size;
    }
    world
        .get_resource::<crate::app::StartOptions>()
        .map_or((1920, 1080), |o| o.0.size)
}

fn finish(world: &mut World, run: BenchRun, cancelled: bool) -> Result<Value, DemoError> {
    if let Some((entity, transform, fov)) = run.saved
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        if let Some(mut t) = e.get_mut::<Transform>() {
            *t = transform;
        }
        if let Some(mut c) = e.get_mut::<Camera>() {
            c.fov_y = fov;
        }
    }
    let machine = machine(world, &run.tier);
    let stamp = Stamp {
        source: TableSource::Demo,
        date: today(),
        commit: option_env!("REGIA_GIT_HASH").unwrap_or("unknown").to_owned(),
        driver: world
            .get_resource::<RenderDevice>()
            .map(|d| AdapterProfile::from_device(d).driver)
            .unwrap_or_default(),
        gpu_index: None,
        gpu_uuid: None,
        size: frame_size(world),
        frames: run.frames,
        warmup: run.warmup,
        cpu: is_software_adapter(&machine.adapter),
        scene: super::scene_dir(world).display().to_string(),
        machine,
    };
    let views: Vec<View> = run.views.iter().map(|v| v.view.clone()).collect();
    let tiers = [run.tier];
    let md_path = run.out.join("showcase-table.md");
    let json_path = run.out.join("showcase-table.json");
    let write = || -> Result<(), String> {
        crate::write_atomic(&md_path, markdown(&stamp, &tiers, &views, &run.rows).as_bytes())?;
        crate::write_atomic(&json_path, json(&stamp, &tiers, &views, &run.rows).as_bytes())
    };
    let error = write().err();
    let result = BenchResult {
        tier: run.tier.name.to_owned(),
        markdown: md_path.clone(),
        json: json_path.clone(),
        rows: run.rows.clone(),
        cancelled,
        error: error.clone(),
    };
    {
        let mut state = world.resource_mut::<DemoState>();
        state.bench.last = Some(result);
        state.bench.runs += 1;
    }
    if let Some(error) = error {
        return Err(DemoError::new(DemoErrorCode::BenchWriteFailed, error));
    }
    tracing::info!(target: "showcase", "demo: benchmark table {}", md_path.display());
    Ok(json!({
        "cancelled": cancelled,
        "markdown": md_path.display().to_string(),
        "json": json_path.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(date_from_unix_days(0), "1970-01-01");
        assert_eq!(date_from_unix_days(11_016), "2000-02-29");
        assert_eq!(date_from_unix_days(20_762), "2026-11-05");
        assert_eq!(date_from_unix_days(-1), "1969-12-31");
    }

    #[test]
    fn every_quality_tier_has_a_table_row() {
        for tier in QualityTier::ALL {
            let row = table_tier(tier);
            assert!(
                regia_render::RenderProfilePreset::from_name(row.preset).is_some(),
                "{}",
                row.preset
            );
        }
        assert_eq!(table_tier(QualityTier::Ultra).name, "ultra");
        assert_eq!(table_tier(QualityTier::Medium).name, "medium");
    }

    /// ADR-5792: every shipped view flies above the example terrain over its whole length. The `low`
    /// view was written for ground at sea level and flew 22 m under the 28 m meadow: its benchmark
    /// row and its leg of the flythrough showed the underside of the terrain.
    #[test]
    fn the_example_views_fly_above_the_example_terrain() {
        const STEPS: u32 = 64;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene");
        let scene = crate::scene::load_scene(&dir).unwrap_or_else(|e| panic!("example scene: {e}"));
        let views = parse_views(&std::fs::read_to_string(dir.join(VIEWS_FILE)).unwrap_or_default())
            .unwrap_or_default();
        assert!(views.len() >= 3);
        for view in &views {
            let sequence = view_sequence(view).unwrap_or_else(|e| panic!("{e}"));
            for step in 0..=STEPS {
                let t = view.seconds * f64::from(step) / f64::from(STEPS);
                let Some(sample) = sequence.sample(t) else {
                    panic!("view `{}` has no camera at {t:.2} s", view.name);
                };
                let ground = scene.map.sample_bilinear(sample.eye.x, sample.eye.z);
                assert!(
                    sample.eye.y >= ground + 1.0,
                    "view `{}` at {t:.2} s: the eye {:?} is not above the ground ({ground:.1} m)",
                    view.name,
                    sample.eye
                );
            }
        }
    }

    #[test]
    fn the_example_views_become_sequences_that_cover_their_length() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene/perf_views.json");
        let views = parse_views(&std::fs::read_to_string(path).unwrap_or_default()).unwrap_or_default();
        assert!(views.len() >= 3);
        for view in &views {
            let sequence = view_sequence(view).unwrap_or_else(|e| panic!("{e}"));
            let start = sequence.sample(0.0).map(|s| s.eye);
            let end = sequence.sample(view.seconds).map(|s| s.eye);
            assert!(start.is_some() && end.is_some());
            assert_ne!(start, end, "{} moves", view.name);
        }
    }

    #[test]
    fn a_bad_path_is_a_message_not_a_panic() {
        let view = View {
            name: "bad".into(),
            params: json!({ "path": { "keys": "nope" } }),
            seconds: 1.0,
        };
        assert!(view_sequence(&view).is_err());
    }
}
