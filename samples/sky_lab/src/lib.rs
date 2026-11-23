// SPDX-License-Identifier: MIT-0
//! Sky Lab: a day cycle and weather presets on the physical sky and volumetric clouds.
//!
//! The scene is a flat plain and one ridge under the sky. A [`Scenario`] moves the sun from
//! a start hour to an end hour over a fixed number of frames while the cloud layer blends from
//! one [`WeatherPreset`] to another. Everything is a function of the frame number, so a run
//! renders the same frames every time: no wall clock, no random input.
//!
//! The model (presets, blending, day clock, command-line options) is plain CPU code and has
//! unit tests that need no GPU. [`lab`] renders it headless, and `tests/smoke.rs` runs 120
//! frames. The agent guide page is `book/agents/sky-lab.md`.

pub mod lab;

use regia_render::{AtmosphereParameters, CloudSettings, OutdoorParams, TimeOfDay, sun_position};

/// Frames in the default run (and in the smoke test).
pub const DEFAULT_FRAMES: u32 = 120;

/// A named cloud layer setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WeatherPreset {
    /// No clouds.
    Clear,
    /// A few scattered cumulus.
    Fair,
    /// Broken cumulus with gaps.
    Broken,
    /// A low, thick, near-complete stratus sheet.
    Overcast,
    /// Dense towering cloud under strong wind.
    Storm,
}

impl WeatherPreset {
    /// Every preset, in the order the command line lists them.
    pub const ALL: [WeatherPreset; 5] = [
        WeatherPreset::Clear,
        WeatherPreset::Fair,
        WeatherPreset::Broken,
        WeatherPreset::Overcast,
        WeatherPreset::Storm,
    ];

    /// Lowercase name used on the command line and in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            WeatherPreset::Clear => "clear",
            WeatherPreset::Fair => "fair",
            WeatherPreset::Broken => "broken",
            WeatherPreset::Overcast => "overcast",
            WeatherPreset::Storm => "storm",
        }
    }

    /// The preset called `name` (see [`WeatherPreset::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.name() == name)
    }

    /// The cloud layer this preset sets. Every preset keeps `altitude_top > altitude_bottom`
    /// and a coverage in `0..=1`.
    #[must_use]
    pub fn clouds(self) -> CloudSettings {
        let base = CloudSettings::default();
        match self {
            WeatherPreset::Clear => CloudSettings {
                coverage: 0.0,
                ..base
            },
            WeatherPreset::Fair => CloudSettings {
                coverage: 0.3,
                cloud_type: 0.5,
                density: 0.04,
                wind_m_per_s: [6.0, 2.0],
                ..base
            },
            WeatherPreset::Broken => CloudSettings {
                coverage: 0.6,
                cloud_type: 0.6,
                density: 0.05,
                wind_m_per_s: [9.0, 3.0],
                ..base
            },
            WeatherPreset::Overcast => CloudSettings {
                coverage: 0.9,
                cloud_type: 0.1,
                density: 0.05,
                altitude_bottom: 900.0,
                altitude_top: 2400.0,
                wind_m_per_s: [4.0, 1.0],
                ..base
            },
            WeatherPreset::Storm => CloudSettings {
                coverage: 0.85,
                cloud_type: 1.0,
                density: 0.08,
                altitude_bottom: 1200.0,
                altitude_top: 6000.0,
                wind_m_per_s: [18.0, 6.0],
                ..base
            },
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Blends two cloud layers: coverage, type, density, altitudes and wind interpolate; the
/// noise scales, resolution, seed and pattern come from `from`. `t` is clamped to `0..=1`.
///
/// The renderer rebuilds its weather map whenever coverage or type change, so a blend in
/// progress rebuilds it every frame (a 128 x 128 map).
#[must_use]
pub fn blend_clouds(from: &CloudSettings, to: &CloudSettings, t: f32) -> CloudSettings {
    let t = t.clamp(0.0, 1.0);
    CloudSettings {
        coverage: lerp(from.coverage, to.coverage, t),
        cloud_type: lerp(from.cloud_type, to.cloud_type, t),
        density: lerp(from.density, to.density, t),
        altitude_bottom: lerp(from.altitude_bottom, to.altitude_bottom, t),
        altitude_top: lerp(from.altitude_top, to.altitude_top, t),
        wind_m_per_s: [
            lerp(from.wind_m_per_s[0], to.wind_m_per_s[0], t),
            lerp(from.wind_m_per_s[1], to.wind_m_per_s[1], t),
        ],
        ..*from
    }
}

/// Everything the renderer needs for one frame of a run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameState {
    /// Frame number, from 0.
    pub frame: u32,
    /// Sun clock.
    pub time: TimeOfDay,
    /// Cloud layer, wind clock included.
    pub clouds: CloudSettings,
    /// Weather blend, `0` the `from` preset and `1` the `to` preset.
    pub blend: f32,
}

impl FrameState {
    /// The atmosphere, sun clock and cloud inputs of this frame (an Earth sky).
    #[must_use]
    pub fn outdoor(&self) -> OutdoorParams {
        OutdoorParams {
            time: Some(self.time),
            clouds: self.clouds,
            planet: AtmosphereParameters::earth(),
            ..OutdoorParams::default()
        }
    }

    /// Sun elevation above the horizon in degrees (negative when the sun is down).
    #[must_use]
    pub fn sun_elevation_deg(&self) -> f32 {
        sun_position(&self.time).elevation_rad.to_degrees()
    }
}

/// A run: a day cycle and a weather change over a fixed number of frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scenario {
    /// Number of frames.
    pub frames: u32,
    /// Hour of day on the first frame.
    pub start_hour: f32,
    /// Hour of day on the last frame.
    pub end_hour: f32,
    /// Preset on the first frame.
    pub from: WeatherPreset,
    /// Preset on the last frame.
    pub to: WeatherPreset,
    /// Seconds the wind clock advances per frame.
    pub seconds_per_frame: f32,
}

impl Default for Scenario {
    /// Dawn to dusk over 120 frames, fair weather turning to a storm.
    fn default() -> Self {
        Self {
            frames: DEFAULT_FRAMES,
            start_hour: 5.5,
            end_hour: 18.5,
            from: WeatherPreset::Fair,
            to: WeatherPreset::Storm,
            seconds_per_frame: 0.5,
        }
    }
}

impl Scenario {
    /// Position of `frame` in the run, `0..=1` (a one-frame run is always at 0).
    fn progress(&self, frame: u32) -> f32 {
        if self.frames <= 1 {
            return 0.0;
        }
        (frame.min(self.frames - 1) as f32) / ((self.frames - 1) as f32)
    }

    /// Hour of day on `frame`, linear from `start_hour` to `end_hour`, wrapped into `0..24`.
    #[must_use]
    pub fn hour_at(&self, frame: u32) -> f32 {
        lerp(self.start_hour, self.end_hour, self.progress(frame)).rem_euclid(24.0)
    }

    /// Weather blend on `frame`: a smoothstep from 0 to 1 over the run.
    #[must_use]
    pub fn blend_at(&self, frame: u32) -> f32 {
        let p = self.progress(frame);
        p * p * (3.0 - 2.0 * p)
    }

    /// The state of `frame`. Frames past the end repeat the last one's blend and hour but keep
    /// the wind clock running.
    #[must_use]
    pub fn state_at(&self, frame: u32) -> FrameState {
        let blend = self.blend_at(frame);
        let mut clouds = blend_clouds(&self.from.clouds(), &self.to.clouds(), blend);
        clouds.time_s = frame as f32 * self.seconds_per_frame;
        FrameState {
            frame,
            time: TimeOfDay {
                hour: self.hour_at(frame),
                ..TimeOfDay::default()
            },
            clouds,
            blend,
        }
    }
}

/// What a command line asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// The run.
    pub scenario: Scenario,
    /// Image size in pixels.
    pub size: (u32, u32),
    /// Directory for PNG frames and `report.json`; `None` renders without writing.
    pub out: Option<std::path::PathBuf>,
    /// A PNG is written every this many frames (frame 0 and the last frame always).
    pub every: u32,
    /// Print the preset names and exit.
    pub list: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scenario: Scenario::default(),
            size: (320, 180),
            out: None,
            every: 20,
            list: false,
        }
    }
}

impl Options {
    /// Parses command-line arguments (without the program name).
    ///
    /// Flags: `--frames N`, `--size WxH`, `--from NAME`, `--to NAME`, `--start-hour H`,
    /// `--end-hour H`, `--out DIR`, `--every N`, `--list`.
    ///
    /// # Errors
    /// A message naming the flag or value that is wrong.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            if flag == "--list" {
                options.list = true;
                continue;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--frames" => options.scenario.frames = parse_number(&flag, &value)?,
                "--every" => options.every = parse_number(&flag, &value)?,
                "--start-hour" => options.scenario.start_hour = parse_number(&flag, &value)?,
                "--end-hour" => options.scenario.end_hour = parse_number(&flag, &value)?,
                "--from" => options.scenario.from = parse_preset(&flag, &value)?,
                "--to" => options.scenario.to = parse_preset(&flag, &value)?,
                "--out" => options.out = Some(value.into()),
                "--size" => {
                    let (w, h) = value
                        .split_once('x')
                        .ok_or_else(|| format!("--size wants WxH, got {value}"))?;
                    options.size = (parse_number("--size", w)?, parse_number("--size", h)?);
                }
                _ => return Err(format!("unknown flag {flag}")),
            }
        }
        if options.scenario.frames == 0 {
            return Err("--frames must be at least 1".into());
        }
        if options.every == 0 {
            return Err("--every must be at least 1".into());
        }
        if options.size.0 == 0 || options.size.1 == 0 || options.size.0 > 4096 || options.size.1 > 4096 {
            return Err("--size must be within 1x1 and 4096x4096".into());
        }
        Ok(options)
    }
}

fn parse_number<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{flag}: {value} is not a valid number"))
}

fn parse_preset(flag: &str, value: &str) -> Result<WeatherPreset, String> {
    WeatherPreset::from_name(value).ok_or_else(|| {
        let names: Vec<&str> = WeatherPreset::ALL.iter().map(|p| p.name()).collect();
        format!("{flag}: unknown preset {value} (one of {})", names.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn preset_names_round_trip() {
        for preset in WeatherPreset::ALL {
            assert_eq!(WeatherPreset::from_name(preset.name()), Some(preset));
        }
        assert_eq!(WeatherPreset::from_name("hail"), None);
    }

    #[test]
    fn every_preset_has_a_valid_layer() {
        for preset in WeatherPreset::ALL {
            let c = preset.clouds();
            assert!((0.0..=1.0).contains(&c.coverage), "{preset:?} coverage");
            assert!((0.0..=1.0).contains(&c.cloud_type), "{preset:?} type");
            assert!(c.altitude_top > c.altitude_bottom, "{preset:?} altitudes");
            assert!(c.density > 0.0, "{preset:?} density");
        }
        assert_eq!(WeatherPreset::Clear.clouds().coverage, 0.0);
        assert!(WeatherPreset::Overcast.clouds().coverage > WeatherPreset::Fair.clouds().coverage);
    }

    #[test]
    fn blend_hits_both_ends_and_stays_between() {
        let a = WeatherPreset::Clear.clouds();
        let b = WeatherPreset::Storm.clouds();
        assert_eq!(blend_clouds(&a, &b, 0.0), a);
        assert_eq!(blend_clouds(&a, &b, 1.0), CloudSettings { seed: a.seed, ..b });
        let mid = blend_clouds(&a, &b, 0.5);
        assert!(mid.coverage > a.coverage && mid.coverage < b.coverage);
        assert!(mid.altitude_top > mid.altitude_bottom);
        assert_eq!(blend_clouds(&a, &b, 7.0), blend_clouds(&a, &b, 1.0));
    }

    #[test]
    fn the_day_runs_from_the_start_hour_to_the_end_hour() {
        let s = Scenario::default();
        assert_eq!(s.hour_at(0), s.start_hour);
        assert!((s.hour_at(s.frames - 1) - s.end_hour).abs() < 1e-4);
        let mut last = -1.0;
        for frame in 0..s.frames {
            let hour = s.hour_at(frame);
            assert!(hour > last, "hours increase at frame {frame}");
            last = hour;
        }
    }

    #[test]
    fn the_hour_wraps_past_midnight() {
        let s = Scenario {
            start_hour: 22.0,
            end_hour: 26.0,
            ..Scenario::default()
        };
        assert!((s.hour_at(s.frames - 1) - 2.0).abs() < 1e-4);
        assert!(s.hour_at(s.frames / 2) < 24.0);
    }

    #[test]
    fn the_sun_rises_then_sets_over_the_default_day() {
        let s = Scenario::default();
        let first = s.state_at(0).sun_elevation_deg();
        let noon = s.state_at(s.frames / 2).sun_elevation_deg();
        let last = s.state_at(s.frames - 1).sun_elevation_deg();
        assert!(noon > 30.0, "noon elevation {noon}");
        assert!(first < noon && last < noon, "{first} {noon} {last}");
    }

    #[test]
    fn the_weather_blend_is_monotonic_and_ends_on_the_target() {
        let s = Scenario::default();
        assert_eq!(s.state_at(0).clouds.coverage, s.from.clouds().coverage);
        assert!((s.state_at(s.frames - 1).clouds.coverage - s.to.clouds().coverage).abs() < 1e-5);
        let mut last = -1.0;
        for frame in 0..s.frames {
            let blend = s.blend_at(frame);
            assert!(blend >= last, "blend never decreases (frame {frame})");
            last = blend;
        }
    }

    #[test]
    fn the_wind_clock_follows_the_frame_number() {
        let s = Scenario::default();
        assert_eq!(s.state_at(0).clouds.time_s, 0.0);
        assert_eq!(s.state_at(10).clouds.time_s, 10.0 * s.seconds_per_frame);
        assert_eq!(
            s.state_at(7),
            s.state_at(7),
            "a frame's state is a pure function of its number"
        );
    }

    #[test]
    fn a_one_frame_run_is_the_start_state() {
        let s = Scenario {
            frames: 1,
            ..Scenario::default()
        };
        assert_eq!(s.hour_at(0), s.start_hour);
        assert_eq!(s.blend_at(0), 0.0);
    }

    #[test]
    fn options_parse_flags() {
        let o = Options::parse(args(&[
            "--frames",
            "30",
            "--size",
            "64x36",
            "--from",
            "clear",
            "--to",
            "overcast",
            "--start-hour",
            "9",
            "--end-hour",
            "15",
            "--out",
            "dir",
            "--every",
            "5",
        ]))
        .expect("valid options");
        assert_eq!(o.scenario.frames, 30);
        assert_eq!(o.size, (64, 36));
        assert_eq!(o.scenario.from, WeatherPreset::Clear);
        assert_eq!(o.scenario.to, WeatherPreset::Overcast);
        assert_eq!(o.scenario.start_hour, 9.0);
        assert_eq!(o.every, 5);
        assert_eq!(o.out.as_deref(), Some(std::path::Path::new("dir")));
        assert!(Options::parse(args(&["--list"])).expect("list").list);
        assert_eq!(Options::parse(args(&[])).expect("defaults"), Options::default());
    }

    #[test]
    fn options_reject_bad_input() {
        for bad in [
            &["--frames", "0"][..],
            &["--frames"],
            &["--frames", "many"],
            &["--size", "64"],
            &["--size", "0x10"],
            &["--size", "9000x10"],
            &["--from", "hail"],
            &["--every", "0"],
            &["--bogus", "1"],
        ] {
            assert!(Options::parse(args(bad)).is_err(), "{bad:?} must be refused");
        }
    }
}
