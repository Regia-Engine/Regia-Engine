// SPDX-License-Identifier: MIT-0
//! `vfx_lab --deck-presets` and `vfx_lab --deck-validate`: the Steam Deck performance-preset
//! validation (ADR-1332).
//!
//! `--deck-presets` runs `vfx_lab --deck-check --preset <name>` once per preset
//! (`regia_steam::DeckPerfPreset`: battery, balanced, performance, fsr1), each in its own process
//! and folder, then validates the folders. `--deck-validate <dir>` validates folders copied back
//! from a Deck on any machine. Both write `deck_presets.json` and `deck_presets.md` into the sweep
//! folder and exit 1 when a preset fails.
//!
//! A preset **fails** when its run is missing or invalid (no report, another preset, another
//! cap, frames run while the window was hidden) or when it does not hold its cap (average fps
//! under 97 % of the cap, more than 2 % missed deadlines). It **warns** on what needs a look but
//! may be the machine's state: hitches, a hot APU, a window that is not the preset's game
//! resolution, an fsr1 run outside gamescope, power over the preset's hint, a cheaper preset that
//! costs more GPU time or power than a dearer one. The thresholds are [`Limits::default`].
//!
//! The power check reads the amdgpu `power1_average` samples of `deck_check.json`. On the Deck APU
//! that is taken to be the package power SteamOS's TDP limit caps; the Deck pass confirms it
//! (`book/user-guide/steam-deck.md`), so a power finding is only ever a warning.

use std::path::Path;

use regia_render::{RenderProfile, SsaoMode, TemporalUpscaler};
use regia_steam::{DeckPanel, DeckPerfKnobs, DeckPerfPreset};
use serde_json::{Value, json};

/// The report file one `--deck-check` run writes.
pub const REPORT_FILE: &str = "deck_check.json";
/// The sweep's machine-readable result.
pub const SWEEP_JSON: &str = "deck_presets.json";
/// The sweep's table, for `docs/progress.md` and the evidence folder.
pub const SWEEP_MD: &str = "deck_presets.md";

/// Copies a preset's knobs onto a render profile: the host side of `DeckPerfState`, as the Steam
/// Deck page of the book shows it.
pub fn apply_knobs(profile: &mut RenderProfile, knobs: &DeckPerfKnobs) {
    profile.render_scale = knobs.render_scale;
    profile.temporal_upscaler = match knobs.upscaler.name() {
        "tsr" => TemporalUpscaler::Tsr,
        "fsr3" => TemporalUpscaler::Fsr3,
        _ => TemporalUpscaler::Taau,
    };
    profile.csm_cascade_count = knobs.csm_cascade_count;
    profile.csm_resolution = knobs.csm_resolution;
    profile.ssao = if knobs.ssao {
        SsaoMode::HalfRes
    } else {
        SsaoMode::Off
    };
    profile.contact_shadows = knobs.contact_shadows;
    profile.bloom = knobs.bloom_mip_count > 0;
    profile.bloom_mip_count = knobs.bloom_mip_count;
    profile.local_shadow_max_lights = knobs.local_shadow_max_lights;
    profile.max_visible_lights = knobs.max_visible_lights;
    profile.frame_cap_fps = Some(knobs.frame_cap.fps());
}

/// The knobs as the report records them.
pub fn knobs_json(knobs: &DeckPerfKnobs) -> Value {
    json!({
        "frame_cap_fps": knobs.frame_cap.fps(),
        "render_scale": knobs.render_scale,
        "upscaler": knobs.upscaler.name(),
        "csm_cascade_count": knobs.csm_cascade_count,
        "csm_resolution": knobs.csm_resolution,
        "ssao": knobs.ssao,
        "contact_shadows": knobs.contact_shadows,
        "bloom_mip_count": knobs.bloom_mip_count,
        "local_shadow_max_lights": knobs.local_shadow_max_lights,
        "max_visible_lights": knobs.max_visible_lights,
        "tdp_hint_watts": knobs.tdp_hint_watts,
    })
}

/// `lcd` or `oled`.
pub const fn panel_name(panel: DeckPanel) -> &'static str {
    match panel {
        DeckPanel::Lcd => "lcd",
        DeckPanel::Oled => "oled",
    }
}

/// The panel called `name` (`lcd`, `oled`; case-insensitive).
pub fn panel_from_name(name: &str) -> Option<DeckPanel> {
    match name.trim().to_ascii_lowercase().as_str() {
        "lcd" => Some(DeckPanel::Lcd),
        "oled" => Some(DeckPanel::Oled),
        _ => None,
    }
}

/// A comma-separated preset list (`battery,balanced`); `all` is every preset in menu order.
pub fn parse_presets(list: &str) -> Result<Vec<DeckPerfPreset>, String> {
    if list.trim() == "all" {
        return Ok(DeckPerfPreset::ALL.to_vec());
    }
    let mut presets = Vec::new();
    for name in list.split(',').filter(|n| !n.trim().is_empty()) {
        let preset = DeckPerfPreset::from_name(name).ok_or_else(|| {
            format!(
                "`{}` is not a preset (battery, balanced, performance, fsr1)",
                name.trim()
            )
        })?;
        if !presets.contains(&preset) {
            presets.push(preset);
        }
    }
    if presets.is_empty() {
        return Err("no preset named".to_owned());
    }
    Ok(presets)
}

/// The validation thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// The average fps must reach this share of the cap.
    pub min_fps_ratio: f64,
    /// Most frames that may miss their deadline (1.25 × the period), %.
    pub max_missed_pct: f64,
    /// Most frames over 50 ms before it is a warning, %.
    pub max_over_50ms_pct: f64,
    /// Peak temperature that warns, °C (the governor's hot step, `regia_steam::deck_prep::HOT_C`).
    pub hot_c: f64,
    /// How far the window may be from the preset's game resolution, pixels per axis.
    pub window_tolerance_px: u32,
    /// How much more a cheaper preset may cost than a dearer one before it warns (1.05 = 5 %).
    pub order_tolerance: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_fps_ratio: 0.97,
            max_missed_pct: 2.0,
            max_over_50ms_pct: 0.2,
            hot_c: f64::from(regia_steam::deck_prep::HOT_C),
            window_tolerance_px: 8,
            order_tolerance: 1.05,
        }
    }
}

/// A preset's outcome, worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verdict {
    /// Held its cap, nothing to look at.
    Pass,
    /// Held its cap; something needs a look.
    Warn,
    /// Did not hold its cap, or the run is not valid.
    Fail,
}

impl Verdict {
    /// `pass`, `warn` or `fail`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One finding: a warning or a failure, with a stable lowercase code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// [`Verdict::Warn`] or [`Verdict::Fail`].
    pub verdict: Verdict,
    /// The code (`cap_missed`, `hidden_frames`, …; listed in the book's Steam Deck page).
    pub code: &'static str,
    /// A sentence with the numbers.
    pub message: String,
}

fn warn(code: &'static str, message: String) -> Finding {
    Finding {
        verdict: Verdict::Warn,
        code,
        message,
    }
}

fn fail(code: &'static str, message: String) -> Finding {
    Finding {
        verdict: Verdict::Fail,
        code,
        message,
    }
}

/// Every finding code, in the order the book lists them.
pub const FINDING_CODES: [&str; 16] = [
    "missing_report",
    "wrong_preset",
    "legacy_report",
    "wrong_cap",
    "hidden_frames",
    "cap_missed",
    "missed_deadlines",
    "hitches",
    "hot",
    "window_size",
    "no_gamescope",
    "panel_mismatch",
    "over_power_hint",
    "no_power_samples",
    "order_gpu_time",
    "order_power",
];

/// The numbers of one `deck_check.json` the validation reads. Every field is optional: older
/// reports lack some, and sysfs may offer nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunNumbers {
    /// The preset the run applied (`None`: a run without `--preset`, the Deck profile).
    pub preset: Option<String>,
    /// The panel the knobs were chosen for.
    pub panel: Option<String>,
    /// The panel the adapter's PCI id says this Deck has.
    pub panel_detected: Option<String>,
    /// The frame cap, fps (`None` = uncapped).
    pub cap_fps: Option<f64>,
    /// Run length, s.
    pub duration_s: Option<f64>,
    /// Average fps.
    pub avg_fps: Option<f64>,
    /// 99th percentile frame time, ms.
    pub p99_ms: Option<f64>,
    /// Frames over 1.25 × the period, %.
    pub missed_pct: Option<f64>,
    /// Frames over 50 ms, %.
    pub over_50ms_pct: Option<f64>,
    /// Mean GPU frame time from timestamps, ms.
    pub gpu_ms: Option<f64>,
    /// Mean of the amdgpu power samples, W.
    pub gpu_power_w: Option<f64>,
    /// Mean of the battery power samples, W (on battery only).
    pub battery_w: Option<f64>,
    /// Hottest sensor reading, °C.
    pub peak_temp_c: Option<f64>,
    /// The window size, pixels.
    pub window: Option<(u32, u32)>,
    /// Frames run while the window was hidden (`None` in reports before that field).
    pub hidden_frames: Option<u64>,
    /// Whether gamescope ran the game.
    pub game_mode: Option<bool>,
}

fn mean_of(samples: &[Value], key: &str) -> Option<f64> {
    let values: Vec<f64> = samples.iter().filter_map(|s| s[key].as_f64()).collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

impl RunNumbers {
    /// Reads a `deck_check.json` document.
    pub fn from_report(report: &Value) -> Self {
        let frames = &report["frames"];
        let samples = report["sensors"]["samples"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let window = report["window"].as_array().and_then(|wh| {
            let w = u32::try_from(wh.first()?.as_u64()?).ok()?;
            let h = u32::try_from(wh.get(1)?.as_u64()?).ok()?;
            Some((w, h))
        });
        Self {
            preset: report["preset"].as_str().map(str::to_owned),
            panel: report["panel"].as_str().map(str::to_owned),
            panel_detected: report["panel_detected"].as_str().map(str::to_owned),
            cap_fps: report["cap_fps"].as_f64(),
            duration_s: report["duration_s"].as_f64(),
            avg_fps: frames["avg_fps"].as_f64(),
            p99_ms: frames["p99_ms"].as_f64(),
            missed_pct: frames["missed_deadline_pct"].as_f64(),
            over_50ms_pct: frames["over_50ms_pct"].as_f64(),
            gpu_ms: report["gpu"]["frame_mean_ms"].as_f64().filter(|ms| *ms > 0.0),
            gpu_power_w: mean_of(samples, "gpu_power_w"),
            battery_w: mean_of(samples, "battery_w"),
            peak_temp_c: report["sensors"]["peak_temp_c"].as_f64(),
            window,
            hidden_frames: report["hidden_frames"].as_u64(),
            game_mode: report["system"]["game_mode"].as_bool(),
        }
    }
}

/// One preset's validation.
#[derive(Debug, Clone, PartialEq)]
pub struct PresetResult {
    /// The preset.
    pub preset: DeckPerfPreset,
    /// The panel it was judged for.
    pub panel: DeckPanel,
    /// The run's numbers (`None` without a report).
    pub numbers: Option<RunNumbers>,
    /// What was found, in check order.
    pub findings: Vec<Finding>,
}

impl PresetResult {
    /// The worst finding's verdict, [`Verdict::Pass`] without findings.
    pub fn verdict(&self) -> Verdict {
        self.findings
            .iter()
            .map(|f| f.verdict)
            .max()
            .unwrap_or(Verdict::Pass)
    }
}

/// Validates one preset's run. `default_panel` is used when the report names none.
pub fn validate_run(
    preset: DeckPerfPreset,
    default_panel: DeckPanel,
    report: Option<&Value>,
    limits: &Limits,
) -> PresetResult {
    let Some(report) = report else {
        return PresetResult {
            preset,
            panel: default_panel,
            numbers: None,
            findings: vec![fail(
                "missing_report",
                format!("no readable {REPORT_FILE} for `{}`", preset.name()),
            )],
        };
    };
    let n = RunNumbers::from_report(report);
    let panel = n
        .panel
        .as_deref()
        .and_then(panel_from_name)
        .unwrap_or(default_panel);
    let knobs = preset.knobs(panel);
    let cap = f64::from(knobs.frame_cap.fps());
    let mut findings = Vec::new();

    match n.preset.as_deref() {
        Some(name) if name == preset.name() => {}
        Some(name) => findings.push(fail(
            "wrong_preset",
            format!("the report is a `{name}` run, not `{}`", preset.name()),
        )),
        None if preset == DeckPerfPreset::Balanced => findings.push(warn(
            "legacy_report",
            "the report names no preset (a run without --preset): judged as balanced, the Deck profile"
                .to_owned(),
        )),
        None => findings.push(fail(
            "wrong_preset",
            format!(
                "the report names no preset; `{}` needs --preset {}",
                preset.name(),
                preset.name()
            ),
        )),
    }
    match n.cap_fps {
        Some(c) if (c - cap).abs() < 0.5 => {}
        Some(c) => findings.push(fail(
            "wrong_cap",
            format!(
                "the run was capped at {c} fps; `{}` on {} caps at {cap}",
                preset.name(),
                panel_name(panel)
            ),
        )),
        None => findings.push(fail(
            "wrong_cap",
            format!("the run was uncapped; `{}` caps at {cap}", preset.name()),
        )),
    }
    if let Some(hidden) = n.hidden_frames.filter(|h| *h > 0) {
        findings.push(fail(
            "hidden_frames",
            format!(
                "{hidden} frames ran with the window hidden (screen blanked or covered): the run is not valid"
            ),
        ));
    }
    match n.avg_fps {
        Some(fps) if fps < cap * limits.min_fps_ratio => findings.push(fail(
            "cap_missed",
            format!(
                "{fps:.1} fps average, under {:.0} % of the {cap} fps cap",
                limits.min_fps_ratio * 100.0
            ),
        )),
        Some(_) => {}
        None => findings.push(fail("cap_missed", "the report has no average fps".to_owned())),
    }
    if let Some(missed) = n.missed_pct.filter(|m| *m > limits.max_missed_pct) {
        findings.push(fail(
            "missed_deadlines",
            format!(
                "{missed:.2} % of frames missed their deadline (limit {} %)",
                limits.max_missed_pct
            ),
        ));
    }
    if let Some(over) = n.over_50ms_pct.filter(|o| *o > limits.max_over_50ms_pct) {
        findings.push(warn(
            "hitches",
            format!(
                "{over:.2} % of frames took over 50 ms (limit {} %)",
                limits.max_over_50ms_pct
            ),
        ));
    }
    if let Some(peak) = n.peak_temp_c.filter(|t| *t >= limits.hot_c) {
        findings.push(warn(
            "hot",
            format!(
                "peak {peak:.0} °C reached the governor's hot step ({} °C): the governor would have lowered the cap",
                limits.hot_c
            ),
        ));
    }
    let (want_w, want_h) = preset.game_resolution();
    if let Some((w, h)) = n.window
        && (w.abs_diff(want_w) > limits.window_tolerance_px
            || h.abs_diff(want_h) > limits.window_tolerance_px)
    {
        findings.push(warn(
            "window_size",
            format!(
                "the window was {w}×{h}, not the preset's {want_w}×{want_h}: run with --fullscreen or in Game Mode"
            ),
        ));
    }
    if preset == DeckPerfPreset::GamescopeFsr1 && n.game_mode != Some(true) {
        findings.push(warn(
            "no_gamescope",
            "fsr1 ran outside gamescope: the numbers leave out gamescope's FSR 1 pass (run it in Game Mode at 960×600)"
                .to_owned(),
        ));
    }
    if let Some(detected) = n.panel_detected.as_deref()
        && panel_from_name(detected).is_some_and(|d| d != panel)
    {
        findings.push(warn(
            "panel_mismatch",
            format!(
                "the knobs were chosen for the {} panel but this Deck has the {detected} panel: rerun with --panel {detected}",
                panel_name(panel)
            ),
        ));
    }
    if let Some(hint) = knobs.tdp_hint_watts {
        match n.gpu_power_w {
            Some(watts) if watts > f64::from(hint) => findings.push(warn(
                "over_power_hint",
                format!("mean APU power {watts:.1} W is over the preset's {hint} W hint"),
            )),
            Some(_) => {}
            None => findings.push(warn(
                "no_power_samples",
                format!("no amdgpu power samples: the {hint} W hint was not checked"),
            )),
        }
    }
    PresetResult {
        preset,
        panel,
        numbers: Some(n),
        findings,
    }
}

/// A whole sweep: every preset's result and the cross-preset checks.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepResult {
    /// The per-preset results, in the order run.
    pub presets: Vec<PresetResult>,
    /// Findings that compare presets (`order_gpu_time`, `order_power`).
    pub findings: Vec<Finding>,
}

impl SweepResult {
    /// The worst verdict of every preset and every cross-preset finding.
    pub fn verdict(&self) -> Verdict {
        self.presets
            .iter()
            .map(PresetResult::verdict)
            .chain(self.findings.iter().map(|f| f.verdict))
            .max()
            .unwrap_or(Verdict::Pass)
    }

    fn numbers(&self, preset: DeckPerfPreset) -> Option<&RunNumbers> {
        self.presets
            .iter()
            .find(|r| r.preset == preset)
            .and_then(|r| r.numbers.as_ref())
    }

    /// The machine-readable result (`deck_presets.json`).
    pub fn to_json(&self, limits: &Limits) -> Value {
        let finding =
            |f: &Finding| json!({ "verdict": f.verdict.name(), "code": f.code, "message": f.message });
        let presets: Vec<Value> = self
            .presets
            .iter()
            .map(|r| {
                let n = r.numbers.clone().unwrap_or_default();
                json!({
                    "preset": r.preset.name(),
                    "panel": panel_name(r.panel),
                    "verdict": r.verdict().name(),
                    "cap_fps": r.preset.knobs(r.panel).frame_cap.fps(),
                    "knobs": knobs_json(&r.preset.knobs(r.panel)),
                    "avg_fps": n.avg_fps,
                    "p99_ms": n.p99_ms,
                    "missed_deadline_pct": n.missed_pct,
                    "over_50ms_pct": n.over_50ms_pct,
                    "gpu_ms": n.gpu_ms,
                    "gpu_power_w": n.gpu_power_w,
                    "battery_w": n.battery_w,
                    "peak_temp_c": n.peak_temp_c,
                    "window": n.window,
                    "game_mode": n.game_mode,
                    "findings": r.findings.iter().map(finding).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "schema": 1,
            "tool": "vfx_lab --deck-presets",
            "verdict": self.verdict().name(),
            "limits": {
                "min_fps_ratio": limits.min_fps_ratio,
                "max_missed_pct": limits.max_missed_pct,
                "max_over_50ms_pct": limits.max_over_50ms_pct,
                "hot_c": limits.hot_c,
                "window_tolerance_px": limits.window_tolerance_px,
                "order_tolerance": limits.order_tolerance,
            },
            "presets": presets,
            "findings": self.findings.iter().map(finding).collect::<Vec<_>>(),
        })
    }

    /// The result as a Markdown table plus the findings (`deck_presets.md`).
    pub fn to_markdown(&self) -> String {
        let num =
            |v: Option<f64>, digits: usize| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.digits$}"));
        let mut out = format!("# Deck preset validation: {}\n\n", self.verdict().name());
        out.push_str(
            "| Preset | Panel | Verdict | Cap | Avg fps | p99 ms | Missed % | GPU ms | APU W | Peak °C |\n",
        );
        out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        for r in &self.presets {
            let n = r.numbers.clone().unwrap_or_default();
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.preset.name(),
                panel_name(r.panel),
                r.verdict().name(),
                r.preset.knobs(r.panel).frame_cap.fps(),
                num(n.avg_fps, 1),
                num(n.p99_ms, 1),
                num(n.missed_pct, 2),
                num(n.gpu_ms, 2),
                num(n.gpu_power_w, 1),
                num(n.peak_temp_c, 0),
            ));
        }
        let all: Vec<(&str, &Finding)> = self
            .presets
            .iter()
            .flat_map(|r| r.findings.iter().map(move |f| (r.preset.name(), f)))
            .chain(self.findings.iter().map(|f| ("sweep", f)))
            .collect();
        if !all.is_empty() {
            out.push_str("\n## Findings\n\n");
            for (scope, f) in all {
                out.push_str(&format!(
                    "- {scope}: {} `{}`: {}\n",
                    f.verdict.name(),
                    f.code,
                    f.message
                ));
            }
        }
        out
    }
}

/// Validates a sweep: each run, then the cross-preset order (a cheaper preset must not cost more
/// GPU time or power than a dearer one, within [`Limits::order_tolerance`]).
pub fn validate_sweep(
    runs: &[(DeckPerfPreset, Option<Value>)],
    default_panel: DeckPanel,
    limits: &Limits,
) -> SweepResult {
    let mut sweep = SweepResult {
        presets: runs
            .iter()
            .map(|(preset, report)| validate_run(*preset, default_panel, report.as_ref(), limits))
            .collect(),
        findings: Vec::new(),
    };
    use DeckPerfPreset::{Balanced, BatterySaver, Performance};
    // Battery saver and performance both render fewer pixels than balanced per frame.
    for (cheap, dear) in [(BatterySaver, Balanced), (Performance, Balanced)] {
        let gpu = |p| sweep.numbers(p).and_then(|n| n.gpu_ms);
        if let (Some(a), Some(b)) = (gpu(cheap), gpu(dear))
            && a > b * limits.order_tolerance
        {
            sweep.findings.push(warn(
                "order_gpu_time",
                format!(
                    "`{}` took {a:.2} ms of GPU per frame, more than `{}` at {b:.2} ms",
                    cheap.name(),
                    dear.name()
                ),
            ));
        }
    }
    // Power follows the pixel rate: battery saver < balanced < performance.
    for (cheap, dear) in [(BatterySaver, Balanced), (Balanced, Performance)] {
        let power = |p| sweep.numbers(p).and_then(|n| n.gpu_power_w);
        if let (Some(a), Some(b)) = (power(cheap), power(dear))
            && a > b * limits.order_tolerance
        {
            sweep.findings.push(warn(
                "order_power",
                format!(
                    "`{}` drew {a:.1} W, more than `{}` at {b:.1} W",
                    cheap.name(),
                    dear.name()
                ),
            ));
        }
    }
    sweep
}

/// Reads `<dir>/<preset>/deck_check.json` for each preset; a missing or unreadable report is
/// `None` (the validation fails that preset, nothing panics).
pub fn load_sweep(dir: &Path, presets: &[DeckPerfPreset]) -> Vec<(DeckPerfPreset, Option<Value>)> {
    presets
        .iter()
        .map(|preset| {
            let report = std::fs::read_to_string(dir.join(preset.name()).join(REPORT_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());
            (*preset, report)
        })
        .collect()
}

/// Writes `deck_presets.json` and `deck_presets.md` into `dir`, each through a temporary file and
/// a rename.
pub fn write_sweep(dir: &Path, sweep: &SweepResult, limits: &Limits) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(&sweep.to_json(limits)).map_err(std::io::Error::other)?;
    for (name, text) in [(SWEEP_JSON, json), (SWEEP_MD, sweep.to_markdown())] {
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, dir.join(name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `deck_check.json` as `--deck-check --preset` writes it, holding its cap.
    fn report(preset: DeckPerfPreset, panel: DeckPanel) -> Value {
        let knobs = preset.knobs(panel);
        let cap = f64::from(knobs.frame_cap.fps());
        let (w, h) = preset.game_resolution();
        json!({
            "schema": 1,
            "tool": "vfx_lab --deck-check",
            "preset": preset.name(),
            "panel": panel_name(panel),
            "panel_detected": panel_name(panel),
            "knobs": knobs_json(&knobs),
            "duration_s": 120.0,
            "window": [w, h],
            "cap_fps": cap,
            "hidden_frames": 0,
            "frames": {
                "frames": (cap * 120.0) as u64,
                "avg_fps": cap,
                "p99_ms": 1000.0 / cap * 1.15,
                "missed_deadline_pct": 0.4,
                "over_50ms_pct": 0.02,
            },
            "gpu": { "timestamps": true, "frame_mean_ms": 10.0 * f64::from(knobs.render_scale) },
            "sensors": {
                "peak_temp_c": 61.0,
                "samples": [
                    { "gpu_power_w": f64::from(knobs.tdp_hint_watts.unwrap_or(15)) - 2.0, "battery_w": null },
                    { "gpu_power_w": f64::from(knobs.tdp_hint_watts.unwrap_or(15)) - 1.0, "battery_w": null },
                ],
            },
            "system": { "game_mode": true },
        })
    }

    fn codes(result: &PresetResult) -> Vec<&'static str> {
        result.findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn a_run_that_holds_its_cap_passes() {
        for preset in DeckPerfPreset::ALL {
            for panel in [DeckPanel::Lcd, DeckPanel::Oled] {
                let r = validate_run(
                    preset,
                    DeckPanel::Lcd,
                    Some(&report(preset, panel)),
                    &Limits::default(),
                );
                assert_eq!(
                    r.verdict(),
                    Verdict::Pass,
                    "{preset:?} {panel:?}: {:?}",
                    r.findings
                );
                assert_eq!(r.panel, panel, "the report's panel wins");
            }
        }
    }

    #[test]
    fn invalid_runs_fail() {
        let limits = Limits::default();
        let r = validate_run(DeckPerfPreset::Performance, DeckPanel::Lcd, None, &limits);
        assert_eq!((r.verdict(), codes(&r)), (Verdict::Fail, vec!["missing_report"]));

        let mut other = report(DeckPerfPreset::BatterySaver, DeckPanel::Lcd);
        let r = validate_run(DeckPerfPreset::Performance, DeckPanel::Lcd, Some(&other), &limits);
        // Another preset's run: its 30 fps cap is not performance's 60, nor does it reach 60.
        assert_eq!(codes(&r), ["wrong_preset", "wrong_cap", "cap_missed"]);

        other["hidden_frames"] = json!(12);
        let r = validate_run(
            DeckPerfPreset::BatterySaver,
            DeckPanel::Lcd,
            Some(&other),
            &limits,
        );
        assert_eq!(codes(&r), ["hidden_frames"]);
        assert_eq!(r.verdict(), Verdict::Fail);

        other["hidden_frames"] = json!(0);
        other["cap_fps"] = Value::Null;
        let r = validate_run(
            DeckPerfPreset::BatterySaver,
            DeckPanel::Lcd,
            Some(&other),
            &limits,
        );
        assert_eq!(codes(&r), ["wrong_cap"], "an uncapped run is not a preset run");
    }

    #[test]
    fn a_missed_cap_fails_and_hitches_or_heat_warn() {
        let limits = Limits::default();
        let mut slow = report(DeckPerfPreset::Performance, DeckPanel::Lcd);
        slow["frames"]["avg_fps"] = json!(55.9);
        slow["frames"]["missed_deadline_pct"] = json!(6.5);
        let r = validate_run(DeckPerfPreset::Performance, DeckPanel::Lcd, Some(&slow), &limits);
        assert_eq!(codes(&r), ["cap_missed", "missed_deadlines"]);
        assert!(
            r.findings[0].message.starts_with("55.9 fps average"),
            "{}",
            r.findings[0].message
        );
        // 58.3 fps is 97.2 % of 60: held.
        slow["frames"]["avg_fps"] = json!(58.3);
        slow["frames"]["missed_deadline_pct"] = json!(1.9);
        slow["frames"]["over_50ms_pct"] = json!(0.5);
        slow["sensors"]["peak_temp_c"] = json!(91.0);
        let r = validate_run(DeckPerfPreset::Performance, DeckPanel::Lcd, Some(&slow), &limits);
        assert_eq!(codes(&r), ["hitches", "hot"]);
        assert_eq!(r.verdict(), Verdict::Warn);
    }

    #[test]
    fn window_gamescope_panel_and_power_warnings() {
        let limits = Limits::default();
        let mut run = report(DeckPerfPreset::GamescopeFsr1, DeckPanel::Lcd);
        run["window"] = json!([1280, 800]);
        run["system"]["game_mode"] = json!(false);
        run["panel_detected"] = json!("oled");
        run["sensors"]["samples"] = json!([{ "gpu_power_w": 14.0 }, { "gpu_power_w": 13.0 }]);
        let r = validate_run(DeckPerfPreset::GamescopeFsr1, DeckPanel::Lcd, Some(&run), &limits);
        assert_eq!(
            codes(&r),
            ["window_size", "no_gamescope", "panel_mismatch", "over_power_hint"]
        );
        assert_eq!(r.verdict(), Verdict::Warn);
        assert!(
            r.findings[3].message.contains("13.5 W"),
            "{}",
            r.findings[3].message
        );
        run["sensors"]["samples"] = json!([{ "gpu_power_w": null }]);
        let r = validate_run(DeckPerfPreset::GamescopeFsr1, DeckPanel::Lcd, Some(&run), &limits);
        assert!(codes(&r).contains(&"no_power_samples"));
        // Performance has no power hint: nothing to check.
        let mut perf = report(DeckPerfPreset::Performance, DeckPanel::Lcd);
        perf["sensors"]["samples"] = json!([]);
        let r = validate_run(DeckPerfPreset::Performance, DeckPanel::Lcd, Some(&perf), &limits);
        assert_eq!(r.verdict(), Verdict::Pass, "{:?}", r.findings);
    }

    #[test]
    fn the_sweep_checks_the_cost_order() {
        let limits = Limits::default();
        let mut runs: Vec<(DeckPerfPreset, Option<Value>)> = DeckPerfPreset::ALL
            .iter()
            .map(|p| (*p, Some(report(*p, DeckPanel::Lcd))))
            .collect();
        let sweep = validate_sweep(&runs, DeckPanel::Lcd, &limits);
        assert_eq!(sweep.verdict(), Verdict::Pass, "{:#?}", sweep.findings);
        // Battery saver drawing more than balanced, and costing more GPU time.
        let battery = runs[0].1.as_mut().expect("battery report");
        battery["sensors"]["samples"] = json!([{ "gpu_power_w": 12.0 }]);
        battery["gpu"]["frame_mean_ms"] = json!(9.0);
        let sweep = validate_sweep(&runs, DeckPanel::Lcd, &limits);
        let found: Vec<&str> = sweep.findings.iter().map(|f| f.code).collect();
        assert_eq!(found, ["order_gpu_time", "order_power"]);
        // 12 W is also over battery saver's 8 W hint: still only warnings.
        assert_eq!(codes(&sweep.presets[0]), ["over_power_hint"]);
        assert_eq!(sweep.verdict(), Verdict::Warn);
    }

    #[test]
    fn the_real_2026_10_14_run_validates_as_balanced() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/deck/2026-10-14/deck_check.json");
        let text = std::fs::read_to_string(path).expect("the Deck evidence is in the repo");
        let real: Value = serde_json::from_str(&text).expect("valid JSON");
        let r = validate_run(
            DeckPerfPreset::Balanced,
            DeckPanel::Lcd,
            Some(&real),
            &Limits::default(),
        );
        // 40.0 fps at a 40 cap, 0.52 % missed, 0.04 % over 50 ms, 53 °C: the cap held. It is a
        // pre-preset desktop run in a 1278×721 window, so it warns but does not fail.
        assert_eq!(codes(&r), ["legacy_report", "window_size"], "{:#?}", r.findings);
        assert_eq!(r.verdict(), Verdict::Warn);
        let n = r.numbers.expect("numbers");
        assert_eq!(n.avg_fps, Some(40.0));
        assert_eq!(n.window, Some((1278, 721)));
        assert!(
            n.gpu_power_w.is_some_and(|w| w > 0.0 && w < 12.0),
            "{:?}",
            n.gpu_power_w
        );
    }

    #[test]
    fn the_sweep_writes_json_and_markdown_and_reads_folders() {
        let dir = regia_test_utils::unique_temp_dir("vfx_lab_deck_presets");
        for preset in [DeckPerfPreset::BatterySaver, DeckPerfPreset::Balanced] {
            let run = dir.join(preset.name());
            std::fs::create_dir_all(&run).expect("test io");
            std::fs::write(
                run.join(REPORT_FILE),
                serde_json::to_string(&report(preset, DeckPanel::Lcd)).expect("test io"),
            )
            .expect("test io");
        }
        std::fs::create_dir_all(dir.join("performance")).expect("test io");
        std::fs::write(dir.join("performance").join(REPORT_FILE), "{ not json").expect("test io");
        let presets = parse_presets("battery,balanced,performance").expect("test io");
        let runs = load_sweep(&dir, &presets);
        assert!(runs[0].1.is_some() && runs[1].1.is_some());
        assert!(runs[2].1.is_none(), "unreadable report");
        let limits = Limits::default();
        let sweep = validate_sweep(&runs, DeckPanel::Lcd, &limits);
        assert_eq!(sweep.verdict(), Verdict::Fail);
        write_sweep(&dir, &sweep, &limits).expect("test io");
        let json: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(SWEEP_JSON)).expect("test io"))
                .expect("test io");
        assert_eq!(json["verdict"], "fail");
        assert_eq!(json["presets"][0]["preset"], "battery");
        assert_eq!(json["presets"][0]["verdict"], "pass");
        assert_eq!(json["presets"][2]["findings"][0]["code"], "missing_report");
        let md = std::fs::read_to_string(dir.join(SWEEP_MD)).expect("test io");
        assert!(md.starts_with("# Deck preset validation: fail\n"), "{md}");
        assert!(md.contains("| battery | lcd | pass | 30 | 30.0 |"), "{md}");
        assert!(md.contains("| performance | lcd | fail | 60 | - |"), "{md}");
        assert!(md.contains("- performance: fail `missing_report`"), "{md}");
        assert!(!dir.join(format!("{SWEEP_JSON}.tmp")).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preset_lists_and_panels_parse() {
        assert_eq!(
            parse_presets("all").expect("test io"),
            DeckPerfPreset::ALL.to_vec()
        );
        assert_eq!(
            parse_presets(" fsr1, battery ,fsr1").expect("test io"),
            vec![DeckPerfPreset::GamescopeFsr1, DeckPerfPreset::BatterySaver]
        );
        assert!(parse_presets("turbo").expect_err("error").contains("turbo"));
        assert!(parse_presets(" , ").is_err());
        assert_eq!(panel_from_name("OLED"), Some(DeckPanel::Oled));
        assert_eq!(panel_from_name("crt"), None);
        for code in FINDING_CODES {
            assert!(code.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{code}");
        }
    }

    #[test]
    fn knobs_land_on_the_render_profile() {
        let mut profile = RenderProfile::from_preset(regia_render::RenderProfilePreset::Deck);
        let knobs = DeckPerfPreset::BatterySaver.knobs(DeckPanel::Lcd);
        apply_knobs(&mut profile, &knobs);
        assert_eq!(profile.render_scale, 0.6);
        assert_eq!(profile.temporal_upscaler, TemporalUpscaler::Taau);
        assert_eq!(profile.csm_resolution, 1024);
        assert_eq!(profile.ssao, SsaoMode::Off);
        assert!(!profile.contact_shadows);
        assert_eq!(profile.frame_cap_fps, Some(30));
        assert!(profile.bloom);
        let knobs = DeckPerfPreset::Balanced.knobs(DeckPanel::Oled);
        apply_knobs(&mut profile, &knobs);
        assert_eq!(profile.temporal_upscaler, TemporalUpscaler::Tsr);
        assert_eq!(profile.ssao, SsaoMode::HalfRes);
        assert_eq!(profile.frame_cap_fps, Some(45));
        assert_eq!(knobs_json(&knobs)["upscaler"], "tsr");
    }
}
