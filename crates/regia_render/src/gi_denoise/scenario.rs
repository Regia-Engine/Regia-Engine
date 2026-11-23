// SPDX-License-Identifier: MPL-2.0
//! Deterministic synthetic GI sequences with a known ground truth, and the runner that scores
//! the denoiser on them ([`run_scenario`]).
//!
//! The view is a wall at 10 m with a floor below the horizon (a crease: the normal turns 90°)
//! and a box in front at 5 m. The wall's irradiance varies smoothly along the world x axis and
//! drops to a quarter in a shadow band (`u mod 64 ≥ 48`, a hard illumination edge on flat
//! geometry); the floor is brighter and shares the band; the box is lit at
//! [`GiScenario::box_irradiance`]. The noisy input is the truth times the mean of
//! [`GiScenario::spp`] unit exponential variates per pixel, one per channel set (a path-traced
//! diffuse bounce at one sample per pixel has about this spread: standard deviation equal to
//! the mean, a long bright tail). The noise comes from a PCG hash of pixel, frame, sample and
//! seed (Jarzynski and Olano, "Hash Functions for GPU Rendering", JCGT 9(3) 2020), so every run
//! is bit-reproducible.
//!
//! | Kind | Motion | What it exercises |
//! |---|---|---|
//! | `static` | none | convergence, flicker, bias, edge leak |
//! | `pan` | the camera pans `pan_px` pixels a frame | reprojection, the column revealed at the screen edge each frame |
//! | `disocclusion` | the box slides `box_speed` pixels a frame over a still wall | history rejection behind the box |
//! | `light_step` | none; the light goes from 25 % to 100 % at `step_frame` | lag after a lighting change |

use super::{
    GiDenoise, GiDenoiseError, GiDenoiseGpu, GiDenoiseInput, GiDenoiseSettings, GiDenoiser, metrics,
};
use crate::RenderDevice;

/// Wall colour (multiplies the irradiance).
const WALL_TINT: [f32; 3] = [1.0, 0.9, 0.8];
/// Floor colour.
const FLOOR_TINT: [f32; 3] = [0.8, 0.9, 1.0];
/// Pixels next to the box (horizontally) that count as its edge band.
const BOX_BAND_PX: i64 = 3;
/// Largest scenario side accepted (keeps an agent call cheap).
pub const MAX_SCENARIO_SIDE: u32 = 512;
/// Most frames a scenario may run.
pub const MAX_SCENARIO_FRAMES: u32 = 512;
/// Most samples per pixel a scenario may average.
pub const MAX_SCENARIO_SPP: u32 = 64;
/// A light-step run has converged when its mean is within this fraction of the truth's.
pub const LAG_TOLERANCE: f32 = 0.1;

/// Which synthetic sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GiScenarioKind {
    /// Still camera, still scene.
    Static,
    /// The camera pans sideways.
    Pan,
    /// The box slides over the wall.
    Disocclusion,
    /// The light jumps from 25 % to 100 %.
    LightStep,
}

impl GiScenarioKind {
    /// Every kind, in report order.
    pub const ALL: [Self; 4] = [Self::Static, Self::Pan, Self::Disocclusion, Self::LightStep];

    /// The protocol name (`static`, `pan`, `disocclusion`, `light_step`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Pan => "pan",
            Self::Disocclusion => "disocclusion",
            Self::LightStep => "light_step",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// One synthetic sequence's parameters. [`Self::new`] gives the regression defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct GiScenario {
    /// Which sequence.
    pub kind: GiScenarioKind,
    /// Frame width, 16..=[`MAX_SCENARIO_SIDE`].
    pub width: u32,
    /// Frame height, 12..=[`MAX_SCENARIO_SIDE`].
    pub height: u32,
    /// Frames to run, 2..=[`MAX_SCENARIO_FRAMES`].
    pub frames: u32,
    /// Frames before the scores start (the history settles), below `frames`.
    pub warmup: u32,
    /// Noise samples averaged per pixel, 1..=[`MAX_SCENARIO_SPP`].
    pub spp: u32,
    /// Noise seed.
    pub seed: u32,
    /// `pan`: pixels the view moves per frame (negative pans the other way).
    pub pan_px: i32,
    /// `disocclusion`: pixels the box moves per frame.
    pub box_speed: i32,
    /// `light_step`: the first frame at full light (after `warmup`, before `frames`).
    pub step_frame: u32,
    /// The box's irradiance (the wall's is about 1).
    pub box_irradiance: f32,
    /// Draw the noise from the spatiotemporal blue-noise table instead of the hash (ADR-2075): what
    /// a gather with blue-noise rotations hands the denoiser. Off by default.
    pub blue_noise: bool,
}

impl GiScenario {
    /// The regression defaults: 64×48, 48 frames, 16 of warm-up, one sample per pixel.
    #[must_use]
    pub fn new(kind: GiScenarioKind) -> Self {
        Self {
            kind,
            width: 64,
            height: 48,
            frames: 48,
            warmup: 16,
            spp: 1,
            seed: 1,
            pan_px: 1,
            box_speed: 1,
            step_frame: 24,
            box_irradiance: 4.0,
            blue_noise: false,
        }
    }

    /// Checks every field's range.
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::InvalidSetting`] naming the field.
    pub fn validate(&self) -> Result<(), GiDenoiseError> {
        let bad = |name: &'static str, reason: String| -> Result<(), GiDenoiseError> {
            Err(GiDenoiseError::InvalidSetting { name, reason })
        };
        if !(16..=MAX_SCENARIO_SIDE).contains(&self.width) {
            return bad(
                "width",
                format!("{} is not in 16..={MAX_SCENARIO_SIDE}", self.width),
            );
        }
        if !(12..=MAX_SCENARIO_SIDE).contains(&self.height) {
            return bad(
                "height",
                format!("{} is not in 12..={MAX_SCENARIO_SIDE}", self.height),
            );
        }
        if !(2..=MAX_SCENARIO_FRAMES).contains(&self.frames) {
            return bad(
                "frames",
                format!("{} is not in 2..={MAX_SCENARIO_FRAMES}", self.frames),
            );
        }
        if self.warmup >= self.frames {
            return bad(
                "warmup",
                format!("{} leaves no frame to score of {}", self.warmup, self.frames),
            );
        }
        if !(1..=MAX_SCENARIO_SPP).contains(&self.spp) {
            return bad("spp", format!("{} is not in 1..={MAX_SCENARIO_SPP}", self.spp));
        }
        if self.kind == GiScenarioKind::LightStep
            && (self.step_frame <= self.warmup || self.step_frame >= self.frames)
        {
            return bad(
                "step_frame",
                format!(
                    "{} must come after warmup {} and before frames {}",
                    self.step_frame, self.warmup, self.frames
                ),
            );
        }
        if self.pan_px.unsigned_abs() > self.width / 4 {
            return bad(
                "pan_px",
                format!("{} is more than a quarter of the width", self.pan_px),
            );
        }
        if self.box_speed.unsigned_abs() > self.width / 4 {
            return bad(
                "box_speed",
                format!("{} is more than a quarter of the width", self.box_speed),
            );
        }
        if !(self.box_irradiance.is_finite() && self.box_irradiance > 0.0) {
            return bad(
                "box_irradiance",
                format!("{} is not above 0", self.box_irradiance),
            );
        }
        Ok(())
    }

    /// The box's left edge (screen x for `disocclusion`, world x otherwise), top, width and
    /// height at frame `t`.
    fn box_rect(&self, t: u32) -> (i64, i64, i64, i64) {
        let w = i64::from(self.width);
        let h = i64::from(self.height);
        let bw = w / 4;
        let bx = if self.kind == GiScenarioKind::Disocclusion {
            w / 8 + i64::from(self.box_speed) * i64::from(t)
        } else {
            w / 2 - bw / 2
        };
        (bx, h / 6, bw, h / 3)
    }

    /// World x of screen column 0 at frame `t`.
    fn offset(&self, t: u32) -> i64 {
        if self.kind == GiScenarioKind::Pan {
            i64::from(self.pan_px) * i64::from(t)
        } else {
            0
        }
    }

    fn light(&self, t: u32) -> f32 {
        if self.kind == GiScenarioKind::LightStep && t < self.step_frame {
            0.25
        } else {
            1.0
        }
    }

    /// Frame `t`: the truth, the noisy input, the guides and the masks the scores use.
    #[must_use]
    pub fn frame(&self, t: u32) -> GiScenarioFrame {
        let w = i64::from(self.width);
        let h = i64::from(self.height);
        let n = (w * h) as usize;
        let (bx, by, bw, bh) = self.box_rect(t);
        let offset = self.offset(t);
        let light = self.light(t);
        let horizon = 2 * h / 3;
        let key = self.seed.wrapping_mul(0x9E37_79B9);
        let in_box = |x: i64, y: i64| {
            let bx_x = if self.kind == GiScenarioKind::Disocclusion {
                x
            } else {
                x + offset
            };
            (bx..bx + bw).contains(&bx_x) && (by..by + bh).contains(&y)
        };
        let mut f = GiScenarioFrame {
            width: self.width,
            height: self.height,
            truth: Vec::with_capacity(n),
            noisy: Vec::with_capacity(n),
            guide: Vec::with_capacity(n),
            motion: Vec::with_capacity(n),
            surface: vec![true; n],
            box_band: Vec::with_capacity(n),
            shadow_edge: Vec::with_capacity(n),
        };
        for y in 0..h {
            for x in 0..w {
                let u = x + offset;
                let floor = y >= horizon;
                let fg = in_box(x, y);
                let near_box = !fg && (1..=BOX_BAND_PX).any(|d| in_box(x - d, y) || in_box(x + d, y));
                let wall = !floor && !fg;
                let z = if fg {
                    5.0
                } else if floor {
                    10.0 - 6.0 * (y - horizon) as f32 / (h - horizon) as f32
                } else {
                    10.0
                };
                let normal = if floor && !fg {
                    [0.0, 1.0, 0.0]
                } else {
                    [0.0, 0.0, 1.0]
                };
                let band = u.rem_euclid(64);
                let shadow = if band >= 48 { 0.25 } else { 1.0 };
                let lit = if fg {
                    self.box_irradiance
                } else if floor {
                    2.0 * shadow
                } else {
                    (1.0 + 0.5 * (u as f32 * (std::f32::consts::TAU / 48.0)).sin()) * shadow
                };
                let e = lit * light;
                let tint = if fg {
                    [1.0; 3]
                } else if floor {
                    FLOOR_TINT
                } else {
                    WALL_TINT
                };
                let truth = tint.map(|c| c * e);
                let idx = (y * w + x) as u32;
                let mut sum = 0.0f32;
                for k in 0..self.spp {
                    let v = idx.wrapping_add(
                        self.width
                            .wrapping_mul(self.height)
                            .wrapping_mul(t.wrapping_mul(self.spp).wrapping_add(k)),
                    );
                    let u01 = if self.blue_noise {
                        crate::blue_noise::scalar(
                            (x as u32).wrapping_add(self.seed.wrapping_mul(17)) & 63,
                            (y as u32).wrapping_add(self.seed.wrapping_mul(29)) & 63,
                            t.wrapping_add(k.wrapping_mul(3)),
                        )
                    } else {
                        (pcg(v ^ key) >> 8) as f32 / 16_777_216.0
                    };
                    sum += -(1.0 - u01).ln();
                }
                let scale = sum / self.spp as f32;
                f.truth.push(truth);
                f.noisy.push(truth.map(|c| c * scale));
                f.guide.push([normal[0], normal[1], normal[2], z]);
                f.motion.push(match self.kind {
                    GiScenarioKind::Pan => [self.pan_px as f32, 0.0],
                    GiScenarioKind::Disocclusion if fg => [-self.box_speed as f32, 0.0],
                    _ => [0.0, 0.0],
                });
                f.box_band.push(near_box && wall);
                let near_shadow_edge = (46..50).contains(&band) || !(2..62).contains(&band);
                f.shadow_edge.push(near_shadow_edge && !near_box && !fg);
            }
        }
        f
    }
}

/// PCG hash (Jarzynski and Olano 2020).
fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// One frame of a scenario.
#[derive(Clone, Debug)]
pub struct GiScenarioFrame {
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// The noise-free irradiance.
    pub truth: Vec<[f32; 3]>,
    /// The noisy input.
    pub noisy: Vec<[f32; 3]>,
    /// Normals and depth ([`GiDenoiseInput::guide`]).
    pub guide: Vec<[f32; 4]>,
    /// Motion to the previous frame ([`GiDenoiseInput::motion`]).
    pub motion: Vec<[f32; 2]>,
    /// Pixels with a surface (all of them in these scenes).
    pub surface: Vec<bool>,
    /// Wall pixels 1–3 px beside the box: where a filter that crosses the depth edge leaks.
    pub box_band: Vec<bool>,
    /// Pixels within 2 px of a shadow edge, away from the box.
    pub shadow_edge: Vec<bool>,
}

impl GiScenarioFrame {
    /// The frame as denoiser input.
    #[must_use]
    pub fn input(&self) -> GiDenoiseInput<'_> {
        GiDenoiseInput {
            width: self.width,
            height: self.height,
            noisy: &self.noisy,
            guide: &self.guide,
            motion: &self.motion,
        }
    }
}

/// The scores of one scenario run. Ratios are relative to the truth's mean luminance; every
/// average is over the scored frames (after `warmup`; for `light_step`, before the step).
#[derive(Clone, Debug, PartialEq)]
pub struct GiDenoiseReport {
    /// The scenario.
    pub scenario: GiScenarioKind,
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Frames run.
    pub frames: u32,
    /// Frames scored.
    pub scored_frames: u32,
    /// [`metrics::relative_rmse`] of the raw input.
    pub raw_rel_rmse: f32,
    /// [`metrics::relative_rmse`] of the output.
    pub rel_rmse: f32,
    /// The worst scored frame's output [`metrics::relative_rmse`].
    pub max_rel_rmse: f32,
    /// [`metrics::temporal_flicker`] of the raw input.
    pub raw_flicker: f32,
    /// [`metrics::temporal_flicker`] of the output (pixels with history only).
    pub flicker: f32,
    /// [`metrics::mean_bias`] of the output.
    pub bias: f32,
    /// [`metrics::relative_mean_abs_error`] of the raw input on the shadow-edge band.
    pub raw_shadow_edge_error: Option<f32>,
    /// The same for the output: how much the filter smears illumination edges.
    pub shadow_edge_error: Option<f32>,
    /// Light leaked across the box's depth edge: the box band's mean output with the box at
    /// [`GiScenario::box_irradiance`] minus the same with the box 8× darker (same noise),
    /// relative to the band's truth. 0 for a filter that never crosses the edge. `None` for
    /// `light_step`.
    pub edge_leak: Option<f32>,
    /// `disocclusion`: output [`metrics::relative_rmse`] on pixels with no history this frame.
    pub disoccluded_rel_rmse: Option<f32>,
    /// `light_step`: frames after the step until the output's mean is within
    /// [`LAG_TOLERANCE`] of the truth's (`None`: never within the run).
    pub lag_frames: Option<u32>,
    /// Mean history length over the scored frames.
    pub mean_history: f32,
}

#[derive(Default)]
struct Mean {
    sum: f64,
    count: u32,
}

impl Mean {
    fn add(&mut self, v: Option<f32>) {
        if let Some(v) = v {
            self.sum += f64::from(v);
            self.count += 1;
        }
    }

    fn get(&self) -> Option<f32> {
        if self.count > 0 {
            Some((self.sum / f64::from(self.count)) as f32)
        } else {
            None
        }
    }
}

/// Runs `scenario` through a [`GiDenoiser`] (the CPU reference) with `settings` and scores it.
///
/// # Errors
///
/// [`GiDenoiseError::InvalidSetting`] from either argument's validation.
pub fn run_scenario(
    scenario: &GiScenario,
    settings: &GiDenoiseSettings,
) -> Result<GiDenoiseReport, GiDenoiseError> {
    run_scenario_with(scenario, || GiDenoiser::new(settings.clone()))
}

/// Runs `scenario` through a [`GiDenoiseGpu`] on `device` and scores it. Blocks on one readback
/// per frame.
///
/// # Errors
///
/// [`GiDenoiseError::InvalidSetting`] from validation, [`GiDenoiseError::Gpu`] from the device.
pub fn run_scenario_gpu(
    scenario: &GiScenario,
    settings: &GiDenoiseSettings,
    device: &RenderDevice,
) -> Result<GiDenoiseReport, GiDenoiseError> {
    run_scenario_with(scenario, || GiDenoiseGpu::new(device, settings.clone()))
}

/// One frame of RGB values, row-major (the previous frame the flicker score compares against).
type RgbFrame = Vec<[f32; 3]>;

/// Runs `scenario` through denoisers `make` builds (one for the run, one for the leak twin)
/// and scores it.
///
/// # Errors
///
/// [`GiDenoiseError::InvalidSetting`] from the scenario's validation, or whatever `make` and
/// the denoiser return.
pub fn run_scenario_with<D: GiDenoise>(
    scenario: &GiScenario,
    mut make: impl FnMut() -> Result<D, GiDenoiseError>,
) -> Result<GiDenoiseReport, GiDenoiseError> {
    let _span = tracing::debug_span!("gi_denoise_scenario", kind = scenario.kind.name()).entered();
    scenario.validate()?;
    let mut denoiser = make()?;
    // The leak twin: the same run with a darker box. Same noise, same geometry.
    let mut twin = if scenario.kind == GiScenarioKind::LightStep {
        None
    } else {
        let dark = GiScenario {
            box_irradiance: scenario.box_irradiance / 8.0,
            ..scenario.clone()
        };
        Some((dark, make()?))
    };
    let mut raw_rmse = Mean::default();
    let mut rmse = Mean::default();
    let mut max_rmse = 0.0f32;
    let mut raw_flicker = Mean::default();
    let mut flicker = Mean::default();
    let mut bias = Mean::default();
    let mut raw_edge = Mean::default();
    let mut edge = Mean::default();
    let mut leak = Mean::default();
    let mut disoccluded = Mean::default();
    let mut history = Mean::default();
    let mut lag = None;
    let mut scored = 0u32;
    let mut previous: Option<(RgbFrame, RgbFrame)> = None;
    for t in 0..scenario.frames {
        let f = scenario.frame(t);
        let out = denoiser.denoise_frame(&f.input())?;
        let twin_out = match twin.as_mut() {
            Some((dark, d)) => Some(d.denoise_frame(&dark.frame(t).input())?),
            None => None,
        };
        if scenario.kind == GiScenarioKind::LightStep && t >= scenario.step_frame && lag.is_none() {
            let got = metrics::mean_bias(&out.color, &f.truth, &f.surface);
            if got.is_some_and(|b| b.abs() < LAG_TOLERANCE) {
                lag = Some(t - scenario.step_frame);
            }
        }
        let scoring =
            t >= scenario.warmup && (scenario.kind != GiScenarioKind::LightStep || t < scenario.step_frame);
        if scoring {
            scored += 1;
            raw_rmse.add(metrics::relative_rmse(&f.noisy, &f.truth, &f.surface));
            let r = metrics::relative_rmse(&out.color, &f.truth, &f.surface);
            max_rmse = max_rmse.max(r.unwrap_or(0.0));
            rmse.add(r);
            bias.add(metrics::mean_bias(&out.color, &f.truth, &f.surface));
            history.add(metrics_mean(&out.history_len));
            if let Some((prev_raw, prev_out)) = previous.as_ref() {
                let settled: Vec<bool> = out.history_len.iter().map(|l| *l > 1.0).collect();
                raw_flicker.add(metrics::temporal_flicker(
                    prev_raw, &f.noisy, &f.motion, &settled, f.width,
                ));
                flicker.add(metrics::temporal_flicker(
                    prev_out, &out.color, &f.motion, &settled, f.width,
                ));
            }
            raw_edge.add(metrics::relative_mean_abs_error(
                &f.noisy,
                &f.truth,
                &f.shadow_edge,
            ));
            edge.add(metrics::relative_mean_abs_error(
                &out.color,
                &f.truth,
                &f.shadow_edge,
            ));
            if let Some(dark) = twin_out.as_ref() {
                let bright = metrics::mean_luminance(&out.color, &f.box_band);
                let darker = metrics::mean_luminance(&dark.color, &f.box_band);
                let truth = metrics::mean_luminance(&f.truth, &f.box_band);
                if let (Some(b), Some(d), Some(band_truth)) = (bright, darker, truth)
                    && band_truth > 0.0
                {
                    leak.add(Some((b - d) / band_truth));
                }
            }
            if scenario.kind == GiScenarioKind::Disocclusion {
                let fresh: Vec<bool> = out.history_len.iter().map(|l| *l > 0.0 && *l < 1.5).collect();
                disoccluded.add(metrics::relative_rmse(&out.color, &f.truth, &fresh));
            }
        }
        previous = Some((f.noisy, out.color));
    }
    Ok(GiDenoiseReport {
        scenario: scenario.kind,
        width: scenario.width,
        height: scenario.height,
        frames: scenario.frames,
        scored_frames: scored,
        raw_rel_rmse: raw_rmse.get().unwrap_or(0.0),
        rel_rmse: rmse.get().unwrap_or(0.0),
        max_rel_rmse: max_rmse,
        raw_flicker: raw_flicker.get().unwrap_or(0.0),
        flicker: flicker.get().unwrap_or(0.0),
        bias: bias.get().unwrap_or(0.0),
        raw_shadow_edge_error: raw_edge.get(),
        shadow_edge_error: edge.get(),
        edge_leak: leak.get(),
        disoccluded_rel_rmse: disoccluded.get(),
        lag_frames: lag,
        mean_history: history.get().unwrap_or(0.0),
    })
}

fn metrics_mean(values: &[f32]) -> Option<f32> {
    if values.is_empty() {
        None
    } else {
        Some((values.iter().map(|v| f64::from(*v)).sum::<f64>() / values.len() as f64) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blue noise as the denoiser's input (ADR-2075): a lower error and less flicker than the hash,
    /// on a static view and under a pan.
    #[test]
    fn blue_noise_input_lowers_the_error_and_the_flicker() {
        for kind in [GiScenarioKind::Static, GiScenarioKind::Pan] {
            let base = GiScenario::new(kind);
            let hash = run_scenario(&base, &GiDenoiseSettings::default()).expect("hash input");
            let blue_sc = GiScenario {
                blue_noise: true,
                ..base.clone()
            };
            let blue = run_scenario(&blue_sc, &GiDenoiseSettings::default()).expect("blue input");
            eprintln!(
                "{}: rel rmse hash {:.4}, blue {:.4}; flicker hash {:.4}, blue {:.4}",
                kind.name(),
                hash.rel_rmse,
                blue.rel_rmse,
                hash.flicker,
                blue.flicker,
            );
            assert!(
                blue.rel_rmse < hash.rel_rmse,
                "{kind:?}: {} vs {}",
                blue.rel_rmse,
                hash.rel_rmse
            );
            assert!(
                blue.flicker < hash.flicker,
                "{kind:?}: {} vs {}",
                blue.flicker,
                hash.flicker
            );
        }
    }

    #[test]
    fn names_round_trip() {
        for kind in GiScenarioKind::ALL {
            assert_eq!(GiScenarioKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(GiScenarioKind::from_name("nope"), None);
    }

    #[test]
    fn frames_are_deterministic_and_the_noise_is_unbiased() {
        let sc = GiScenario::new(GiScenarioKind::Static);
        let a = sc.frame(3);
        let b = sc.frame(3);
        assert_eq!(a.noisy, b.noisy);
        assert_ne!(a.noisy, sc.frame(4).noisy, "each frame draws new noise");
        // Mean of the noise multiplier over many pixels and frames is 1 (a unit exponential).
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for t in 0..8 {
            let f = sc.frame(t);
            for (n, r) in f.noisy.iter().zip(&f.truth) {
                sum += f64::from(n[1] / r[1]);
                count += 1;
            }
        }
        let mean = sum / count as f64;
        assert!((mean - 1.0).abs() < 0.02, "noise mean {mean}");
    }

    #[test]
    fn scene_layout() {
        let sc = GiScenario::new(GiScenarioKind::Static);
        let f = sc.frame(0);
        let at = |x: usize, y: usize| y * 64 + x;
        // The box (x 24..40, y 8..24) is at 5 m, the wall at 10 m, the floor (y >= 32) turns up.
        assert_eq!(f.guide[at(30, 10)][3], 5.0);
        assert_eq!(f.guide[at(5, 10)][3], 10.0);
        assert_eq!(f.guide[at(5, 40)][1], 1.0);
        assert!(f.box_band[at(22, 10)] && f.box_band[at(41, 10)] && !f.box_band[at(20, 10)]);
        assert!(f.shadow_edge[at(47, 4)] && f.shadow_edge[at(0, 4)] && !f.shadow_edge[at(10, 4)]);
        // The shadow band (x 48..64) is a quarter as bright as the lit wall beside it.
        assert!(f.truth[at(50, 4)][0] < 0.5 * f.truth[at(44, 4)][0]);
    }

    #[test]
    fn motion_matches_the_geometry() {
        // Pan: what pixel x shows at frame t, pixel x + pan showed at t - 1.
        let sc = GiScenario::new(GiScenarioKind::Pan);
        let (a, b) = (sc.frame(4), sc.frame(5));
        for (i, m) in b.motion.iter().enumerate() {
            let x = i % 64;
            let px = x as i64 + m[0] as i64;
            if px < 64 {
                assert_eq!(b.truth[i], a.truth[i - x + px as usize], "pixel {i}");
            }
        }
        // Disocclusion: the box moved right, its pixels point back left onto the box; the
        // wall column it uncovered points at itself, where the box was.
        let sc = GiScenario::new(GiScenarioKind::Disocclusion);
        let (a, b) = (sc.frame(4), sc.frame(5));
        let mut uncovered = 0;
        for (i, m) in b.motion.iter().enumerate() {
            let j = (i as i64 + m[0] as i64) as usize;
            if b.guide[i][3] == 5.0 {
                assert_eq!(a.guide[j][3], 5.0, "box pixel {i}");
            } else if a.guide[j][3] == 5.0 {
                uncovered += 1;
            }
        }
        assert_eq!(
            uncovered, 16,
            "one column of the 16-pixel-tall box is uncovered per frame"
        );
    }

    #[test]
    fn invalid_scenarios_are_rejected() {
        let mut sc = GiScenario::new(GiScenarioKind::LightStep);
        sc.step_frame = sc.warmup;
        assert!(matches!(
            sc.validate(),
            Err(GiDenoiseError::InvalidSetting {
                name: "step_frame",
                ..
            })
        ));
        let mut sc = GiScenario::new(GiScenarioKind::Static);
        sc.width = 8;
        assert!(matches!(
            sc.validate(),
            Err(GiDenoiseError::InvalidSetting { name: "width", .. })
        ));
        let mut sc = GiScenario::new(GiScenarioKind::Static);
        sc.warmup = sc.frames;
        assert!(matches!(
            sc.validate(),
            Err(GiDenoiseError::InvalidSetting { name: "warmup", .. })
        ));
        let mut sc = GiScenario::new(GiScenarioKind::Pan);
        sc.pan_px = 40;
        assert!(matches!(
            sc.validate(),
            Err(GiDenoiseError::InvalidSetting { name: "pan_px", .. })
        ));
    }
}
