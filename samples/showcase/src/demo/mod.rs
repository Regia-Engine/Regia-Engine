// SPDX-License-Identifier: MIT-0
//! The interactive demo (`showcase --demo`, ADR-3013): the showcase scene with a free-fly camera, time-of-day and
//! weather sliders, a quality-tier picker with automatic detection that says why it chose a tier, a measured
//! frames-per-second overlay, a screenshot key, a benchmark button that writes the performance table and a
//! credits page read from the scene's asset manifest. Offline: no accounts, no network, nothing sent anywhere.
//!
//! Every control goes through one path, [`apply`] on a [`DemoCommand`]: the on-screen panel ([`ui`]), the keys
//! and the agent methods (`agent`, `demo.*`, `book/agents/showcase-demo.md`) all end there, so a button, a key
//! and an agent call behave and fail alike. The agent methods exist only with the `agent` feature (the default);
//! the shipped demo builds `--no-default-features --features demo` and has none (ADR-5419).
//!
//! - [`fly`]: keyboard, mouse and gamepad into the engine's free camera (`regia_framework::camera`).
//! - [`quality`]: the tier picker over `regia_render::quality` and the plain-words reason.
//! - [`sun`]: the sun on tiers that draw no physical sky.
//! - [`fps`]: the measured frame-rate overlay.
//! - [`capture`]: screenshots to PNG files.
//! - [`bench`](mod@bench): the benchmark flythrough over `perf_views.json`, written by [`crate::perf_table`].
//! - [`credits`]: the scene's `asset_manifest.json`.

#[cfg(feature = "agent")]
pub mod agent;
pub mod bench;
pub mod capture;
pub mod credits;
pub mod fly;
pub mod fps;
pub mod quality;
pub mod sun;
pub mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use regia::prelude::*;
use regia_render::QualityTier;
use regia_scene::level::{LevelSettings, LevelTimeOfDay};
use regia_vfx::{Weather, WeatherState};

use crate::app::ShowcaseScene;

/// A stable, machine-readable reason a demo control refused (lowercase ASCII, digits and `_`; listed in
/// `book/agents/showcase-demo.md`). The agent methods carry it in the error's `data.code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DemoErrorCode {
    /// A number is out of range or not finite, or a parameter has the wrong type.
    BadValue,
    /// The tier name is not one of the picker's.
    UnknownTier,
    /// The weather preset is not one the engine knows.
    UnknownWeather,
    /// The page name is not `controls`, `credits` or `hidden`.
    UnknownPage,
    /// A benchmark is already running.
    BenchRunning,
    /// No benchmark is running (cancel).
    BenchIdle,
    /// The scene has no `perf_views.json`.
    NoViews,
    /// `perf_views.json` does not parse, or a view is too short for the asked frames.
    ViewsMalformed,
    /// The app has no renderer or GPU device yet (detection, offscreen screenshot).
    NoRenderer,
    /// The screenshot could not be captured or written.
    ScreenshotFailed,
    /// The benchmark table could not be written.
    BenchWriteFailed,
}

impl DemoErrorCode {
    /// Every code, in the order the book page lists them.
    pub const ALL: [DemoErrorCode; 11] = [
        DemoErrorCode::BadValue,
        DemoErrorCode::UnknownTier,
        DemoErrorCode::UnknownWeather,
        DemoErrorCode::UnknownPage,
        DemoErrorCode::BenchRunning,
        DemoErrorCode::BenchIdle,
        DemoErrorCode::NoViews,
        DemoErrorCode::ViewsMalformed,
        DemoErrorCode::NoRenderer,
        DemoErrorCode::ScreenshotFailed,
        DemoErrorCode::BenchWriteFailed,
    ];

    /// The code as text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DemoErrorCode::BadValue => "demo_bad_value",
            DemoErrorCode::UnknownTier => "demo_unknown_tier",
            DemoErrorCode::UnknownWeather => "demo_unknown_weather",
            DemoErrorCode::UnknownPage => "demo_unknown_page",
            DemoErrorCode::BenchRunning => "demo_bench_running",
            DemoErrorCode::BenchIdle => "demo_bench_idle",
            DemoErrorCode::NoViews => "demo_no_views",
            DemoErrorCode::ViewsMalformed => "demo_views_malformed",
            DemoErrorCode::NoRenderer => "demo_no_renderer",
            DemoErrorCode::ScreenshotFailed => "demo_screenshot_failed",
            DemoErrorCode::BenchWriteFailed => "demo_bench_write_failed",
        }
    }
}

/// A refused demo control: a stable [`DemoErrorCode`] and a message.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{}: {}", .code.as_str(), .message)]
pub struct DemoError {
    /// Why.
    pub code: DemoErrorCode,
    /// A human-readable detail.
    pub message: String,
}

impl DemoError {
    /// An error of `code`.
    pub fn new(code: DemoErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Which page the panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DemoPage {
    /// The controls: sliders, the tier picker, screenshot, benchmark and credits buttons.
    #[default]
    Controls,
    /// The credits page.
    Credits,
    /// Only the frame-rate overlay (the picture alone).
    Hidden,
}

impl DemoPage {
    /// Every page.
    pub const ALL: [DemoPage; 3] = [DemoPage::Controls, DemoPage::Credits, DemoPage::Hidden];

    /// The page's name (`controls`, `credits`, `hidden`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            DemoPage::Controls => "controls",
            DemoPage::Credits => "credits",
            DemoPage::Hidden => "hidden",
        }
    }

    /// The page for a name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// How screenshots are captured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureMode {
    /// The window's next presented frame, as shown (the windowed demo).
    #[default]
    Window,
    /// An offscreen render of the current frame at this size (a headless app: tests and scripts).
    Offscreen(u32, u32),
}

/// How the demo starts.
#[derive(Clone, Debug, PartialEq)]
pub struct DemoOptions {
    /// Where screenshots (`screenshots/`) and benchmark tables (`benchmark/`) go.
    pub out_dir: PathBuf,
    /// The tier to start with; `None` detects one from the GPU on the first frame.
    pub tier: Option<QualityTier>,
    /// Warm-up frames per benchmark view (rendered, not measured).
    pub bench_warmup: u32,
    /// Measured frames per benchmark view.
    pub bench_frames: u32,
    /// Build the on-screen panel (off in CPU-only tests that have no UI plugin).
    pub ui: bool,
    /// How screenshots are captured.
    pub capture: CaptureMode,
    /// Windowed demo only: quit with exit code 0 after this many frames drawn once the loading splash is gone
    /// (`--exit-after-frames`); 0 runs until the player quits. The headless demo counts every frame instead
    /// (`crate::app::ExitAfter`), since it has no splash.
    pub exit_after_frames: u64,
}

impl Default for DemoOptions {
    fn default() -> Self {
        Self {
            out_dir: default_out_dir(),
            tier: None,
            bench_warmup: bench::DEFAULT_WARMUP,
            bench_frames: bench::DEFAULT_FRAMES,
            ui: true,
            capture: CaptureMode::Window,
            exit_after_frames: 0,
        }
    }
}

/// The demo's output folder when none is given: `<user data dir>/demo` (`~/.local/share/RegiaEngine/<exe>/demo`
/// on Linux, `%APPDATA%\RegiaEngine\<exe>\demo` on Windows), else `regia-demo` in the temp directory.
#[must_use]
pub fn default_out_dir() -> PathBuf {
    #[cfg(not(target_os = "android"))]
    if let Some(dir) = regia_platform::paths::user_dir() {
        return dir.join("demo");
    }
    std::env::temp_dir().join("regia-demo")
}

/// The weather the sliders show: the cloud cover, the rain or snow and the wind speed, plus the preset last
/// picked (cleared when a slider moves).
#[derive(Clone, Debug, PartialEq)]
pub struct WeatherSliders {
    /// The preset last picked, `None` after a slider changed a value.
    pub preset: Option<String>,
    /// Cloud cover, `0..=1`.
    pub coverage: f32,
    /// Rain or snow intensity, `0..=1`.
    pub precipitation: f32,
    /// Wind speed at cloud height, meters per second (`0..=MAX_WIND_M_PER_S`).
    pub wind_m_per_s: f32,
}

/// The fastest wind the slider offers, meters per second.
pub const MAX_WIND_M_PER_S: f32 = 30.0;

impl WeatherSliders {
    /// The sliders of a weather state.
    #[must_use]
    pub fn of(state: &WeatherState) -> Self {
        Self {
            preset: None,
            coverage: state.coverage,
            precipitation: state.precipitation,
            wind_m_per_s: (state.wind_m_per_s[0].hypot(state.wind_m_per_s[1])).min(MAX_WIND_M_PER_S),
        }
    }

    /// `base` with the sliders' values (the wind keeps `base`'s direction, or blows along +X when it had none).
    #[must_use]
    pub fn apply_to(&self, base: &WeatherState) -> WeatherState {
        let [x, z] = base.wind_m_per_s;
        let len = x.hypot(z);
        let dir = if len > 1e-4 {
            [x / len, z / len]
        } else {
            [1.0, 0.0]
        };
        WeatherState {
            coverage: self.coverage,
            precipitation: self.precipitation,
            wind_m_per_s: [dir[0] * self.wind_m_per_s, dir[1] * self.wind_m_per_s],
            ..*base
        }
    }
}

/// Where the tier came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TierSource {
    /// The engine's GPU table picked it ([`quality::detect_words`]).
    Detected,
    /// The player (or an agent) picked it.
    Picked,
    /// The command line asked for it.
    CommandLine,
    /// Nothing has picked one yet (no GPU device seen).
    #[default]
    Pending,
}

impl TierSource {
    /// The source's name (`detected`, `picked`, `command_line`, `pending`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            TierSource::Detected => "detected",
            TierSource::Picked => "picked",
            TierSource::CommandLine => "command_line",
            TierSource::Pending => "pending",
        }
    }
}

/// Everything the panel shows and the agent's `demo.state` reports.
#[derive(Resource, Clone, Debug)]
pub struct DemoState {
    /// The page the panel shows.
    pub page: DemoPage,
    /// The sun clock the time-of-day sliders set (`LevelSettings::atmosphere.time_of_day`).
    pub time_of_day: LevelTimeOfDay,
    /// The weather sliders.
    pub weather: WeatherSliders,
    /// The quality tier the renderer runs.
    pub tier: QualityTier,
    /// Where [`Self::tier`] came from.
    pub tier_source: TierSource,
    /// The last detection, in words ([`quality::TierWords`]).
    pub detection: Option<quality::TierWords>,
    /// Screenshots written so far.
    pub screenshots: u32,
    /// The last screenshot written.
    pub last_screenshot: Option<PathBuf>,
    /// Why the last screenshot failed.
    pub screenshot_error: Option<String>,
    /// The benchmark's progress or last result.
    pub bench: bench::BenchStatus,
    /// The scene's credits.
    pub credits: credits::Credits,
    /// Updates since the demo started.
    pub frame: u64,
}

/// The options the demo was built with.
#[derive(Resource, Clone, Debug)]
pub struct DemoSettings(pub DemoOptions);

/// One control of the demo. [`apply`] runs it; the panel, the keys and the agent methods build them.
#[derive(Clone, Debug, PartialEq)]
pub enum DemoCommand {
    /// Set the sun clock. `None` keeps a field.
    SetTimeOfDay {
        /// Local solar hour, `0..=24`.
        hour: Option<f32>,
        /// Day of the year, `1..=365`.
        day_of_year: Option<u16>,
        /// Latitude, degrees, `-90..=90`.
        latitude_deg: Option<f32>,
    },
    /// Set the weather: a preset, then any slider values on top. `None` keeps a field.
    SetWeather {
        /// A preset of `regia_vfx::WEATHER_PRESETS`.
        preset: Option<String>,
        /// Cloud cover, `0..=1`.
        coverage: Option<f32>,
        /// Rain or snow, `0..=1`.
        precipitation: Option<f32>,
        /// Wind speed, meters per second.
        wind_m_per_s: Option<f32>,
        /// Seconds the change takes (`0`: at once).
        transition_s: f32,
    },
    /// Run this tier (the picker).
    SetTier(QualityTier),
    /// Detect a tier from the GPU and run it (the picker's "Auto").
    DetectTier,
    /// Write a screenshot (`None`: a new numbered file under `<out>/screenshots`).
    Screenshot(Option<PathBuf>),
    /// Start the benchmark flythrough (`None` fields: the options' defaults; `out`: `<out>/benchmark`).
    StartBenchmark {
        /// Warm-up frames per view.
        warmup: Option<u32>,
        /// Measured frames per view.
        frames: Option<u32>,
        /// The folder the table is written to.
        out: Option<PathBuf>,
    },
    /// Stop the running benchmark (its table is written with the unmeasured views `not run`).
    CancelBenchmark,
    /// Show a page.
    ShowPage(DemoPage),
}

/// Commands queued by the panel and the keys, applied by `apply_queued` in `Update`.
#[derive(Resource, Default, Debug)]
pub struct DemoQueue(pub Vec<DemoCommand>);

/// Errors of queued commands (the panel shows the last; the agent sees them in `demo.state`).
#[derive(Resource, Default, Debug)]
pub struct DemoLastError(pub Option<DemoError>);

/// The interactive demo on top of [`crate::app::ShowcasePlugin`] (which the caller adds first).
pub struct DemoPlugin {
    /// How the demo starts.
    pub options: DemoOptions,
}

impl Plugin for DemoPlugin {
    fn build(&self, app: &mut App) {
        let scene_dir = app
            .world()
            .get_resource::<ShowcaseScene>()
            .map(|s| s.0.dir.clone())
            .unwrap_or_default();
        let credits = credits::Credits::load(&scene_dir);
        let tod = app
            .world()
            .get_resource::<LevelSettings>()
            .and_then(|l| l.atmosphere.time_of_day)
            .unwrap_or_default();
        let weather = app
            .world()
            .get_resource::<Weather>()
            .map(|w| WeatherSliders::of(&w.current()))
            .unwrap_or_else(|| WeatherSliders::of(&WeatherState::FAIR));
        let (tier, tier_source) = match self.options.tier {
            Some(tier) => (tier, TierSource::CommandLine),
            None => (QualityTier::High, TierSource::Pending),
        };
        app.insert_resource(DemoState {
            page: DemoPage::Controls,
            time_of_day: tod,
            weather,
            tier,
            tier_source,
            detection: None,
            screenshots: 0,
            last_screenshot: None,
            screenshot_error: None,
            bench: bench::BenchStatus::default(),
            credits,
            frame: 0,
        })
        .insert_resource(DemoSettings(self.options.clone()))
        .init_resource::<DemoQueue>()
        .init_resource::<DemoLastError>()
        .init_resource::<fps::FpsMeter>()
        .init_resource::<fps::FrameClock>()
        .init_resource::<capture::PendingShots>()
        // The scene's scripted weather changes would undo the sliders: the demo owns the weather.
        .insert_resource(crate::app::DirectorWeatherOff);
        // The time of day lives on the level: make sure the level holds one so the renderer follows the slider.
        if let Some(mut level) = app.world_mut().get_resource_mut::<LevelSettings>() {
            level.atmosphere.time_of_day = Some(tod);
        }
        if let Some(tier) = self.options.tier {
            app.add_systems(Startup, move |world: &mut World| quality::apply_tier(world, tier));
        }
        fly::install(app);
        // The agent methods and the generic `quality.get_detected` answer (ADR-5131) only exist with the agent
        // protocol; the shipped demo build compiles them out (ADR-5419).
        #[cfg(feature = "agent")]
        {
            app.init_resource::<regia_agent::HostQualityDetection>();
            agent::install(app.world_mut());
            app.add_systems(Last, quality::publish_detection.after(count_frame));
        }
        app.add_systems(
            Update,
            (
                quality::detect_on_first_device,
                fly::hotkeys,
                apply_queued,
                sun::follow_clock,
                sun::draw_precipitation_once,
                capture::collect_pending,
            )
                .chain(),
        )
        .add_systems(
            PostUpdate,
            bench::drive
                .after(regia_framework::camera::CameraSet::Update)
                .before(regia_framework::camera::CameraSet::Director),
        )
        .add_systems(Last, (fps::measure, bench::measure, count_frame).chain());
        if self.options.ui {
            ui::install(app);
        }
    }
}

fn count_frame(mut state: ResMut<DemoState>) {
    state.frame += 1;
}

/// `Update`: applies the queued commands; a refusal is kept in [`DemoLastError`].
fn apply_queued(world: &mut World) {
    let queued = std::mem::take(&mut world.resource_mut::<DemoQueue>().0);
    for command in queued {
        if let Err(error) = apply(world, command) {
            tracing::warn!(target: "showcase", "demo: {error}");
            world.resource_mut::<DemoLastError>().0 = Some(error);
        }
    }
}

fn check_range(name: &str, value: f32, low: f32, high: f32) -> Result<f32, DemoError> {
    if value.is_finite() && (low..=high).contains(&value) {
        Ok(value)
    } else {
        Err(DemoError::new(
            DemoErrorCode::BadValue,
            format!("{name} must be within {low}..={high}, got {value}"),
        ))
    }
}

/// Runs one command on the world and returns what it did as JSON (the agent methods' answer).
///
/// # Errors
/// A [`DemoError`] naming why the command was refused; nothing changes then.
pub fn apply(world: &mut World, command: DemoCommand) -> Result<serde_json::Value, DemoError> {
    use serde_json::json;
    match command {
        DemoCommand::SetTimeOfDay {
            hour,
            day_of_year,
            latitude_deg,
        } => {
            let mut tod = world.resource::<DemoState>().time_of_day;
            if let Some(hour) = hour {
                tod.hour = check_range("hour", hour, 0.0, 24.0)?;
            }
            if let Some(day) = day_of_year {
                if !(1..=365).contains(&day) {
                    return Err(DemoError::new(
                        DemoErrorCode::BadValue,
                        format!("day_of_year must be within 1..=365, got {day}"),
                    ));
                }
                tod.day_of_year = day;
            }
            if let Some(lat) = latitude_deg {
                tod.latitude_deg = check_range("latitude_deg", lat, -90.0, 90.0)?;
            }
            world.resource_mut::<DemoState>().time_of_day = tod;
            if let Some(mut level) = world.get_resource_mut::<LevelSettings>() {
                level.atmosphere.time_of_day = Some(tod);
            }
            Ok(json!({ "hour": tod.hour, "day_of_year": tod.day_of_year, "latitude_deg": tod.latitude_deg }))
        }
        DemoCommand::SetWeather {
            preset,
            coverage,
            precipitation,
            wind_m_per_s,
            transition_s,
        } => {
            let transition_s = check_range("transition_s", transition_s, 0.0, 600.0)?;
            let base = world
                .get_resource::<Weather>()
                .map_or(WeatherState::FAIR, |w| w.target());
            let mut state = match &preset {
                Some(name) => WeatherState::preset(name).ok_or_else(|| {
                    DemoError::new(
                        DemoErrorCode::UnknownWeather,
                        format!(
                            "unknown weather preset `{name}` (one of {})",
                            regia_vfx::WEATHER_PRESETS.join(", ")
                        ),
                    )
                })?,
                None => base,
            };
            let mut sliders = WeatherSliders::of(&state);
            sliders.preset.clone_from(&preset);
            if let Some(v) = coverage {
                sliders.coverage = check_range("coverage", v, 0.0, 1.0)?;
                sliders.preset = None;
            }
            if let Some(v) = precipitation {
                sliders.precipitation = check_range("precipitation", v, 0.0, 1.0)?;
                sliders.preset = None;
            }
            if let Some(v) = wind_m_per_s {
                sliders.wind_m_per_s = check_range("wind_m_per_s", v, 0.0, MAX_WIND_M_PER_S)?;
                sliders.preset = None;
            }
            state = sliders.apply_to(&state);
            match world.get_resource_mut::<Weather>() {
                Some(mut weather) => {
                    if transition_s > 0.0 {
                        weather.transition_to(state, transition_s);
                    } else {
                        weather.set_now(state);
                    }
                }
                None => {
                    world.insert_resource(Weather::new(state));
                }
            }
            let out = json!({
                "preset": sliders.preset,
                "coverage": sliders.coverage,
                "precipitation": sliders.precipitation,
                "wind_m_per_s": sliders.wind_m_per_s,
                "transition_s": transition_s,
            });
            world.resource_mut::<DemoState>().weather = sliders;
            Ok(out)
        }
        DemoCommand::SetTier(tier) => {
            quality::apply_tier(world, tier);
            let mut state = world.resource_mut::<DemoState>();
            state.tier = tier;
            state.tier_source = TierSource::Picked;
            Ok(json!({ "tier": tier.name(), "source": TierSource::Picked.name() }))
        }
        DemoCommand::DetectTier => {
            let words = quality::detect_now(world)?;
            quality::apply_tier(world, words.tier);
            let out = json!({ "tier": words.tier.name(), "source": TierSource::Detected.name(), "why": words.words, "reason": words.reason });
            let mut state = world.resource_mut::<DemoState>();
            state.tier = words.tier;
            state.tier_source = TierSource::Detected;
            state.detection = Some(words);
            Ok(out)
        }
        DemoCommand::Screenshot(path) => capture::take(world, path),
        DemoCommand::StartBenchmark { warmup, frames, out } => bench::start(world, warmup, frames, out),
        DemoCommand::CancelBenchmark => bench::cancel(world),
        DemoCommand::ShowPage(page) => {
            world.resource_mut::<DemoState>().page = page;
            Ok(json!({ "page": page.name() }))
        }
    }
}

/// The showcase scene and the demo as one plugin (the windowed runner hands the engine one game plugin; the
/// demo reads what the scene plugin inserted, so it is built second).
pub struct ShowcaseDemoPlugin {
    /// The loaded scene.
    pub scene: Arc<crate::scene::SceneData>,
    /// Rendering options.
    pub options: crate::app::Options,
    /// How the demo starts.
    pub demo: DemoOptions,
}

impl Plugin for ShowcaseDemoPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::app::ShowcasePlugin {
            scene: Arc::clone(&self.scene),
            options: self.options,
        });
        app.add_plugins(DemoPlugin {
            options: self.demo.clone(),
        });
    }
}

/// Runs the windowed demo until it is closed: the showcase window (wall clock, the agent endpoint) with the
/// demo on top. Screenshots capture the window.
pub fn run_windowed_demo(
    scene: Arc<crate::scene::SceneData>,
    options: crate::app::Options,
    project_dir: Option<PathBuf>,
    window: crate::app::WindowOptions,
    demo: DemoOptions,
) -> crate::app::AppExit {
    let demo = DemoOptions {
        capture: CaptureMode::Window,
        ..demo
    };
    let exit_after = demo.exit_after_frames;
    regia::run_with(ShowcaseDemoPlugin { scene, options, demo }, move |app| {
        crate::app::configure_windowed(app, project_dir, window, Vec::new());
        configure_loading(app);
        if exit_after > 0 {
            app.add_plugins(ExitAfterSceneFrames(exit_after));
        }
    })
}

/// The windowed demo's `--exit-after-frames N` (found on the Steam Deck pass, ADR-5452): quits with
/// [`crate::app::AppExit::Success`] after `N` frames drawn once the loading splash is gone, so a scripted start of
/// the shipped build (which has no agent to ask it to quit) shows `N` frames of the forest and exits 0. Frames
/// under the splash are not counted: on a cold shader cache the splash alone can last thousands of frames.
pub struct ExitAfterSceneFrames(pub u64);

impl Plugin for ExitAfterSceneFrames {
    fn build(&self, app: &mut App) {
        let limit = self.0;
        app.add_systems(
            Update,
            move |mut count: Local<u64>,
                  splash: Option<Res<regia_render::BootSplash>>,
                  mut exit: MessageWriter<crate::app::AppExit>| {
                if counts_as_scene_frame(splash.as_deref()) {
                    *count += 1;
                    if *count >= limit {
                        exit.write(crate::app::AppExit::Success);
                    }
                }
            },
        );
    }
}

/// Whether this frame shows the scene: no loading splash, or one that is gone.
fn counts_as_scene_frame(splash: Option<&regia_render::BootSplash>) -> bool {
    splash.is_none_or(regia_render::BootSplash::is_done)
}

/// The loading screen's text (the splash's 5×7 font draws upper case).
pub const LOADING_TEXT: &str = "REGIA FOREST DEMO - LOADING";

/// How long the warm of the first demo frame may run while it keeps building pipelines (ADR-5131): a software
/// rasterizer compiles the forest's first frame for about a minute, and abandoning that warm would compile the rest on
/// the frame thread. The engine default (30 s) suits a game behind a menu, not a demo whose first screen is the scene.
pub const LOADING_WARM_MAX: std::time::Duration = std::time::Duration::from_secs(600);

/// Longest gap between two pipelines of a running first-frame warm before it counts as hung, and its plain timeout
/// (queued plus running) before that check applies (ADR-5131; the engine defaults are 8 s and 15 s, less than one slow
/// compile, or one queued map job, on a software rasterizer).
pub const LOADING_WARM_PROGRESS_GRACE: std::time::Duration = std::time::Duration::from_secs(120);

/// The windowed demo's loading screen (ADR-5131): a boot splash with no cap covers the window until the terrain is
/// built on a warm worker ([`crate::app::WarmTerrainBuild`]) and the boot tier is warm; its finish asks the engine to
/// warm the first demo frame on a worker ([`regia_render::GameFrameWarm`]), which holds the splash until that frame's
/// pipelines exist. So the first frame the player sees compiles nothing on the frame thread, and the window keeps
/// presenting (and answering the OS) while the pipelines compile.
pub fn configure_loading(app: &mut App) {
    app.insert_resource(
        regia_render::BootSplash::new()
            .with_text(LOADING_TEXT)
            .with_max_duration(None),
    );
    app.init_resource::<crate::app::WarmTerrainBuild>();
    // Not left to the render plugin: called before it, the limits below would be skipped without a trace (its own
    // `init_resource` keeps these).
    app.init_resource::<regia_render::GameFrameWarm>();
    if let Some(mut warm) = app.world_mut().get_resource_mut::<regia_render::GameFrameWarm>() {
        warm.on_splash_finish = true;
        // `timeout` also bounds the time the warm waits in the queue behind the map tier's other jobs: the engine's
        // 15 s would cancel it on a software rasterizer and leave the first frame to the frame thread.
        warm.timeout = warm.timeout.max(LOADING_WARM_PROGRESS_GRACE);
        warm.max_timeout = warm.max_timeout.max(LOADING_WARM_MAX);
        warm.progress_grace = warm.progress_grace.max(LOADING_WARM_PROGRESS_GRACE);
    }
    app.add_systems(Update, finish_loading);
}

/// `Update` (windowed demo): finishes the loading splash once the terrain is installed (or failed to build) and the
/// warm queue's boot tier is done (the UI overlay's pipelines are part of it).
fn finish_loading(
    stats: Res<crate::app::ShowcaseStats>,
    splash: Option<ResMut<regia_render::BootSplash>>,
    queue: Option<Res<regia_render::PipelineWarmQueue>>,
) {
    let Some(mut splash) = splash else {
        return;
    };
    if splash.is_finishing() || splash.is_done() {
        return;
    }
    if loading_done(&stats, queue.as_deref()) {
        tracing::info!(target: "showcase", "demo loaded: the first frame warms on a worker behind the splash");
        splash.finish();
    }
}

/// Whether the windowed demo's loading screen may finish ([`configure_loading`]).
#[must_use]
pub fn loading_done(
    stats: &crate::app::ShowcaseStats,
    queue: Option<&regia_render::PipelineWarmQueue>,
) -> bool {
    let terrain = stats.terrain_installed || stats.terrain_error.is_some();
    terrain && queue.is_none_or(|q| q.is_tier_ready(regia_render::WarmTier::Boot))
}

/// The headless app ([`crate::app::build_headless_app`]) with the demo on top: screenshots render offscreen at the
/// options' size, the clock is unpaused at a fixed 1/60 s per update (so the camera, the benchmark and the
/// frame-rate overlay advance on `App::update`), and the frame clock is the wall clock unless the caller swaps
/// [`fps::FrameClock`].
///
/// # Errors
/// The device error as text when the machine has no usable Vulkan adapter (not even lavapipe).
pub fn build_headless_demo(
    scene: Arc<crate::scene::SceneData>,
    options: crate::app::Options,
    project_dir: Option<PathBuf>,
    demo: DemoOptions,
) -> Result<App, String> {
    let mut app = crate::app::build_headless_app(scene, options, project_dir)?;
    let demo = DemoOptions {
        capture: CaptureMode::Offscreen(options.size.0, options.size.1),
        ..demo
    };
    app.add_plugins(DemoPlugin { options: demo });
    let mut time = Time::default();
    time.paused = false;
    app.insert_resource(time);
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_nanos(16_666_667),
    ));
    Ok(app)
}

/// The scene directory the app runs (empty when the app has no showcase scene).
#[must_use]
pub fn scene_dir(world: &World) -> PathBuf {
    world
        .get_resource::<ShowcaseScene>()
        .map(|s| s.0.dir.clone())
        .unwrap_or_default()
}

/// The scene's shared data, when the app has one.
#[must_use]
pub fn scene(world: &World) -> Option<Arc<crate::scene::SceneData>> {
    world.get_resource::<ShowcaseScene>().map(|s| Arc::clone(&s.0))
}

/// Where the demo looks for a scene when `--demo` names none: `example_scene` beside the executable, then the
/// sample's own `example_scene` (a source checkout), else `None` (the caller writes the generated example).
#[must_use]
pub fn find_default_scene() -> Option<PathBuf> {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("example_scene")));
    // Runtime path (ADR-10370): the compile-time manifest dir would embed the build machine.
    let source = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(|dir| PathBuf::from(dir).join("example_scene"))
        .or_else(|| {
            let root = std::env::current_exe()
                .ok()?
                .parent()?
                .parent()?
                .parent()?
                .to_path_buf();
            let scene = root.join("samples").join("showcase").join("example_scene");
            scene.is_dir().then_some(scene)
        });
    [beside_exe, source]
        .into_iter()
        .flatten()
        .find(|dir| dir.join("scene.ron").is_file())
}

/// Test fixtures shared by the demo's unit tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A state with no scene, the High tier and the default sun and weather.
    pub(crate) fn state() -> DemoState {
        DemoState {
            page: DemoPage::Controls,
            time_of_day: LevelTimeOfDay::default(),
            weather: WeatherSliders::of(&WeatherState::FAIR),
            tier: QualityTier::High,
            tier_source: TierSource::Pending,
            detection: None,
            screenshots: 0,
            last_screenshot: None,
            screenshot_error: None,
            bench: bench::BenchStatus::default(),
            credits: credits::Credits::default(),
            frame: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--exit-after-frames` in a window counts only frames after the loading splash (ADR-5452): an app whose
    /// splash is showing never quits, and one whose splash is gone quits on exactly the Nth frame.
    #[test]
    fn the_windowed_exit_counts_frames_after_the_splash() {
        fn exits(app: &mut App) -> usize {
            usize::from(app.should_exit().is_some())
        }
        let mut app = App::new();
        app.add_message::<crate::app::AppExit>();
        app.add_plugins(ExitAfterSceneFrames(3));
        app.insert_resource(regia_render::BootSplash::new());
        for _ in 0..10 {
            app.update();
            assert_eq!(exits(&mut app), 0, "a frame under the splash was counted");
        }
        let mut splash = app.world_mut().resource_mut::<regia_render::BootSplash>();
        splash.finish();
        let t = std::time::Instant::now();
        splash.advance(t);
        splash.advance(t + std::time::Duration::from_secs(60));
        assert!(splash.is_done());
        app.update();
        app.update();
        assert_eq!(exits(&mut app), 0, "quit before the third scene frame");
        app.update();
        assert_eq!(exits(&mut app), 1, "no quit on the third scene frame");

        // No splash at all (the headless shape): every frame counts.
        assert!(counts_as_scene_frame(None));
        assert!(!counts_as_scene_frame(Some(&regia_render::BootSplash::new())));
    }

    #[test]
    fn codes_are_lowercase_unique_and_listed() {
        let mut seen = std::collections::HashSet::new();
        for code in DemoErrorCode::ALL {
            let text = code.as_str();
            assert!(
                text.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{text}"
            );
            assert!(seen.insert(text), "{text} twice");
        }
    }

    #[test]
    fn pages_round_trip_by_name() {
        for page in DemoPage::ALL {
            assert_eq!(DemoPage::from_name(page.name()), Some(page));
        }
        assert_eq!(DemoPage::from_name("menu"), None);
    }

    #[test]
    fn weather_sliders_keep_the_wind_direction() {
        let base = WeatherState {
            wind_m_per_s: [0.0, -4.0],
            ..WeatherState::FAIR
        };
        let sliders = WeatherSliders {
            preset: None,
            coverage: 0.7,
            precipitation: 0.2,
            wind_m_per_s: 10.0,
        };
        let state = sliders.apply_to(&base);
        assert!((state.wind_m_per_s[0]).abs() < 1e-5);
        assert!((state.wind_m_per_s[1] + 10.0).abs() < 1e-5);
        assert!((state.coverage - 0.7).abs() < 1e-6);
        let calm = WeatherState {
            wind_m_per_s: [0.0, 0.0],
            ..WeatherState::FAIR
        };
        assert_eq!(sliders.apply_to(&calm).wind_m_per_s, [10.0, 0.0]);
        let back = WeatherSliders::of(&state);
        assert!((back.wind_m_per_s - 10.0).abs() < 1e-4);
    }
}
