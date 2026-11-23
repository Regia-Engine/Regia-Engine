// SPDX-License-Identifier: MPL-2.0
//! Locomotion authoring in the animation editor (ADR-8806, gaps G10 and W5 of the locomotion
//! build-out): Inspector sections for [`GaitRig`] and [`FootPlacement`], and a locomotion test
//! bed that walks a gait over ground presets before the game runs.
//!
//! - **Inspector sections** ([`gait_rig_section`], [`foot_placement_section`]): the settings
//!   with their documented ranges ([`GAIT_SETTING_RANGES`], [`FOOT_SETTING_RANGES`]), leg chain
//!   pickers over the rig's joint tree, gait pattern presets, and the gait asset picker that
//!   loads a `.gait.ron` into the rig or saves the rig's settings through the format's writer
//!   ([`GaitFile::from_params`], atomic write). Every edit is one whole-component
//!   `SetProperty` on the editor's undo stack, exactly like an Inspector field edit.
//! - **Test bed** ([`LocomotionTestBed`]): the stepper (`regia_animation::gait`, driven mode)
//!   on a [`GroundPreset`] (flat, slope, stairs, seeded rubble, a two-layer tunnel) under a
//!   [`LocomotionDriver`] at a set speed and turn rate, played or stepped at a fixed 1/60 s.
//!   It measures the metrics `anim.gait_simulate` (ADR-8708, `book/agents/locomotion-quality.md`)
//!   reports, with the same definitions: largest planted slide, largest foot jump, forced steps,
//!   stability margin ([`LocomotionMetrics`]). The overlay is the `locomotion` gizmo group's own
//!   drawing ([`draw_gait`], ADR-8729) projected into the panel.
//! - **Agent**: `editor.anim_locomotion_preview`, `editor.anim_gait_load` and
//!   `editor.anim_gait_save` (`book/agents/animation-editor.md`).
//!
//! The test bed is presentation tooling: deterministic for identical inputs on one target (the
//! tests compare runs bit for bit), never read by a simulation checksum.

use std::path::Path;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_animation::SkinnedMesh;
use regia_animation::foot_ik::{FootLegDesc, FootPlacement};
use regia_animation::gait::{
    GaitBody, GaitError, GaitGround, GaitMode, GaitParams, GaitPatternSet, GaitPosture, GaitState, GroundHit,
    MAX_LEGS, leg_array, step_gait_driven,
};
use regia_animation::gait_asset::{GaitAsset, GaitAssetError, GaitFile, GaitPatternSpec, gait_error_code};
use regia_animation::gait_rig::{GaitLeg, GaitRig};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{PartialReflect, Reflect, TypePath};
use regia_framework::locomotion_gizmos::{
    LocomotionGizmoColors, LocomotionGizmoStats, LocomotionGizmos, draw_gait,
};
use regia_render::gizmos::GizmoBuffer;

use crate::command::{EditorCommand as _, SetProperty};

/// The test bed's fixed step, seconds (the agent simulate methods' default `dt`).
pub const PREVIEW_DT: f32 = 1.0 / 60.0;

/// Longest test bed run, seconds (18 000 steps at [`PREVIEW_DT`]; `anim.gait_simulate` stops at
/// 20 000).
pub const MAX_PREVIEW_SECONDS: f32 = 300.0;

/// Fastest driver speed, meters per second.
pub const MAX_DRIVER_SPEED: f32 = 20.0;

/// Fastest driver turn rate, radians per second (one turn per second).
pub const MAX_DRIVER_TURN_RATE: f32 = core::f32::consts::TAU;

/// Length of the straight driver's path, meters (a power of two, so the path math is exact).
const STRAIGHT_PATH: f32 = 4096.0;

/// Height of every preset's base layer, meters.
const BASE_HEIGHT: f32 = 0.0;

// ---------------------------------------------------------------------------------------------
// Ground presets

/// A synthetic ground for the test bed. The walker heads along −Z (the object convention's
/// forward), so slopes and stairs climb as it walks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum GroundPreset {
    /// A level floor at height 0.
    #[default]
    Flat,
    /// A plane climbing `degrees` along −Z (0 to 60).
    Slope {
        /// Incline in degrees.
        degrees: f32,
    },
    /// Stairs climbing along −Z: treads `run` meters deep, each `rise` meters higher.
    Stairs {
        /// Step height, meters (0 to 1).
        rise: f32,
        /// Tread depth, meters (0.05 to 10).
        run: f32,
    },
    /// Square blocks `cell` meters wide at seeded heights in `[0, amplitude)`.
    Rubble {
        /// Height range, meters (0 to 2).
        amplitude: f32,
        /// Block width, meters (0.05 to 10).
        cell: f32,
        /// Seed of the block heights.
        seed: u64,
    },
    /// Two layers: the tunnel floor at height 0 and the ground above it at `depth` meters. A
    /// walker in the tunnel must keep its feet on the floor (the stepper's ground band picks the
    /// layer near the body, ADR-8792).
    Tunnel {
        /// Height of the upper layer above the floor, meters (0.5 to 50).
        depth: f32,
    },
}

/// Mixes a seed and a cell index into 64 well-spread bits (the SplitMix64 finalizer, the same
/// mixing `anim.gait_simulate`'s rubble uses, so both grounds agree block for block).
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl GroundPreset {
    /// Every preset with its default values, in menu order.
    #[must_use]
    pub fn presets() -> [Self; 5] {
        [
            Self::Flat,
            Self::Slope { degrees: 10.0 },
            Self::Stairs { rise: 0.15, run: 0.3 },
            Self::Rubble {
                amplitude: 0.1,
                cell: 0.4,
                seed: 7,
            },
            Self::Tunnel { depth: 3.0 },
        ]
    }

    /// The preset's name (`flat`, `slope`, `stairs`, `rubble`, `tunnel`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Slope { .. } => "slope",
            Self::Stairs { .. } => "stairs",
            Self::Rubble { .. } => "rubble",
            Self::Tunnel { .. } => "tunnel",
        }
    }

    /// Checks the documented ranges.
    ///
    /// # Errors
    ///
    /// A message naming the field out of range.
    pub fn validate(&self) -> Result<(), String> {
        let check = |name: &str, v: f32, lo: f32, hi: f32| {
            if v.is_finite() && (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(format!("`{name}` must be a number in {lo}..={hi}"))
            }
        };
        match *self {
            Self::Flat => Ok(()),
            Self::Slope { degrees } => check("degrees", degrees, 0.0, 60.0),
            Self::Stairs { rise, run } => {
                check("rise", rise, 0.0, 1.0)?;
                check("run", run, 0.05, 10.0)
            }
            Self::Rubble { amplitude, cell, .. } => {
                check("amplitude", amplitude, 0.0, 2.0)?;
                check("cell", cell, 0.05, 10.0)
            }
            Self::Tunnel { depth } => check("depth", depth, 0.5, 50.0),
        }
    }

    /// The layer heights under world `(x, z)` (one, or two for the tunnel) and the surface
    /// normal. Flat, slope, stairs and rubble use exactly the expressions of the
    /// `anim.gait_simulate` grounds (`slope {rise_z: -tan}`, `steps {rise: -rise, axis: z}`).
    fn layers(&self, x: f32, z: f32) -> ([f32; 2], usize, [f32; 3]) {
        // The base height is a variable (not folded away) so `0 + v` keeps the agent's bits.
        let height = BASE_HEIGHT;
        match *self {
            Self::Flat => ([height, 0.0], 1, [0.0, 1.0, 0.0]),
            Self::Slope { degrees } => {
                let (rise_x, rise_z) = (0.0_f32, -degrees.to_radians().tan());
                let n = Vec3::new(-rise_x, 1.0, -rise_z).normalize();
                ([height + rise_x * x + rise_z * z, 0.0], 1, n.to_array())
            }
            Self::Stairs { rise, run } => {
                let rise = -rise;
                ([height + rise * (z / run).floor(), 0.0], 1, [0.0, 1.0, 0.0])
            }
            Self::Rubble {
                amplitude,
                cell,
                seed,
            } => {
                let ix = (x / cell).floor() as i64 as u64;
                let iz = (z / cell).floor() as i64 as u64;
                let bits = mix(seed ^ mix(ix ^ mix(iz)));
                let unit = (bits >> 40) as f32 / (1u64 << 24) as f32;
                ([height + amplitude * unit, 0.0], 1, [0.0, 1.0, 0.0])
            }
            Self::Tunnel { depth } => ([height, depth], 2, [0.0, 1.0, 0.0]),
        }
    }

    /// The height of the layer nearest `expected_y` under world `(x, z)`.
    #[must_use]
    pub fn height_near(&self, x: f32, z: f32, expected_y: f32) -> f32 {
        let (heights, count, _) = self.layers(x, z);
        let mut best = heights[0];
        for &h in &heights[1..count] {
            if (h - expected_y).abs() < (best - expected_y).abs() {
                best = h;
            }
        }
        best
    }
}

impl GaitGround for GroundPreset {
    fn sample(&self, x: f32, z: f32, expected_y: f32, _band: f32) -> Option<GroundHit> {
        let (_, _, normal) = self.layers(x, z);
        Some(GroundHit {
            y: self.height_near(x, z, expected_y),
            normal,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Driver and metrics

/// The driven body of the test bed: it starts at the origin heading −Z and moves at `speed`,
/// turning at `turn_rate` (positive turns left, +yaw). The pose is closed form in time, so no
/// error accumulates over a long run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocomotionDriver {
    /// Ground speed, meters per second (0 to [`MAX_DRIVER_SPEED`]).
    pub speed: f32,
    /// Turn rate, radians per second (magnitude at most [`MAX_DRIVER_TURN_RATE`]).
    pub turn_rate: f32,
}

impl Default for LocomotionDriver {
    fn default() -> Self {
        Self {
            speed: 1.0,
            turn_rate: 0.0,
        }
    }
}

impl LocomotionDriver {
    /// Checks the documented ranges.
    ///
    /// # Errors
    ///
    /// A message naming the field out of range.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.speed.is_finite() && (0.0..=MAX_DRIVER_SPEED).contains(&self.speed)) {
            return Err(format!("`speed` must be a number in 0..={MAX_DRIVER_SPEED} m/s"));
        }
        if !(self.turn_rate.is_finite() && self.turn_rate.abs() <= MAX_DRIVER_TURN_RATE) {
            return Err(format!(
                "`turn_rate` must be a number in -{MAX_DRIVER_TURN_RATE}..={MAX_DRIVER_TURN_RATE} rad/s"
            ));
        }
        Ok(())
    }

    /// Body XZ, yaw and XZ velocity at `time` seconds. Going straight it is the
    /// `anim.gait_simulate` body path `[[0, 0], [0, -4096]]` at `speed`, step for step.
    #[must_use]
    pub fn at(&self, time: f32) -> ([f32; 2], f32, [f32; 2]) {
        if self.turn_rate == 0.0 {
            // The polyline formula of the agent's body path, so a straight run matches it bit
            // for bit.
            let (dx, dz) = (0.0_f32, -STRAIGHT_PATH);
            let length = (dx * dx + dz * dz).sqrt();
            let yaw = (-dx).atan2(-dz);
            let left = (self.speed * time).max(0.0);
            if left <= length {
                let t = left / length;
                let velocity = [dx / length * self.speed, dz / length * self.speed];
                let (ax, az) = (BASE_HEIGHT, BASE_HEIGHT);
                return ([ax + dx * t, az + dz * t], yaw, velocity);
            }
            return ([0.0, -STRAIGHT_PATH], yaw, [0.0, 0.0]);
        }
        // A circle of radius speed / turn_rate: forward is [-sin yaw, -cos yaw].
        let yaw = self.turn_rate * time;
        let radius = self.speed / self.turn_rate;
        let (sin, cos) = yaw.sin_cos();
        (
            [radius * (cos - 1.0), -radius * sin],
            yaw,
            [-self.speed * sin, -self.speed * cos],
        )
    }
}

/// Quality metrics of a test bed run, with the definitions of `anim.gait_simulate`'s metrics
/// (`book/agents/locomotion-quality.md`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocomotionMetrics {
    /// Steps run.
    pub steps: u64,
    /// Largest XZ move of a foot planted (and not swinging) before and after one step, meters
    /// (`max_planted_slip`).
    pub max_slide: f32,
    /// Largest 3D move of any foot in one step, meters (`max_foot_jump`).
    pub max_foot_jump: f32,
    /// Fewest planted feet after a step.
    pub min_planted: usize,
    /// Feet forced to step by the reach limit so far ([`GaitState::forced_steps`]).
    pub forced_steps: u32,
    /// Refused footsteps so far ([`GaitState::refused`]).
    pub refused: u32,
    /// Feet re-planted on a better foothold so far ([`GaitState::replants`]).
    pub replants: u32,
    /// The current stability margin, meters ([`GaitState::stability_margin`]; may be infinite).
    pub stability_margin: f32,
    /// The smallest finite stability margin seen, meters (infinite when none was finite).
    pub min_stability_margin: f32,
    /// Whether the walker is settled (idle with every foot at neutral).
    pub settled: bool,
}

impl LocomotionMetrics {
    fn new(legs: usize) -> Self {
        Self {
            steps: 0,
            max_slide: 0.0,
            max_foot_jump: 0.0,
            min_planted: legs,
            forced_steps: 0,
            refused: 0,
            replants: 0,
            stability_margin: f32::INFINITY,
            min_stability_margin: f32::INFINITY,
            settled: false,
        }
    }

    /// Folds one step in (no allocation).
    fn observe(&mut self, before: &GaitState, after: &GaitState) {
        let legs = after.leg_count();
        let mut planted = 0;
        for (i, (a, b)) in before.feet.iter().zip(&after.feet).enumerate().take(legs) {
            let d = [0, 1, 2].map(|k| b.position[k] - a.position[k]);
            let jump = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            self.max_foot_jump = self.max_foot_jump.max(jump);
            if a.planted && b.planted && !before.is_swinging(i) && !after.is_swinging(i) {
                self.max_slide = self.max_slide.max((d[0] * d[0] + d[2] * d[2]).sqrt());
            }
            if b.planted {
                planted += 1;
            }
        }
        self.min_planted = self.min_planted.min(planted);
        if after.stability_margin.is_finite() {
            self.min_stability_margin = self.min_stability_margin.min(after.stability_margin);
        }
        self.steps += 1;
        self.forced_steps = after.forced_steps;
        self.refused = after.refused;
        self.replants = after.replants;
        self.stability_margin = after.stability_margin;
        self.settled = after.settled;
    }
}

// ---------------------------------------------------------------------------------------------
// Test bed

/// Where the test bed's gait settings came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum LocomotionSource {
    /// The built-in six-leg tripod ([`default_gait`]).
    #[default]
    Default,
    /// An entity's `GaitRig` (entity bits), copied when the run started.
    Entity(u64),
    /// A `.gait.ron` file, read when the run started.
    Asset(String),
}

/// Why a test bed step or start failed: the stepper's error and the step it happened at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocomotionError {
    /// The stepper error.
    pub error: GaitError,
    /// The step index (`None`: while building the first stance).
    pub step: Option<u64>,
}

impl LocomotionError {
    /// The agent error code (`gait_settings`, `gait_pattern`, `gait_input`, ...).
    #[must_use]
    pub fn code(&self) -> &'static str {
        gait_error_code(self.error)
    }
}

impl std::fmt::Display for LocomotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.step {
            Some(step) => write!(f, "{} at step {step}", self.error),
            None => write!(f, "{} in the first stance", self.error),
        }
    }
}

/// The locomotion test bed: one walker on one ground under one driver. Not undoable (like the
/// preview's parameter sliders it is a harness, not an edit).
#[derive(Clone, Debug)]
pub struct LocomotionTestBed {
    /// Where `params` came from.
    pub source: LocomotionSource,
    /// The gait settings being previewed.
    pub params: GaitParams,
    /// The ground.
    pub ground: GroundPreset,
    /// The driven body.
    pub driver: LocomotionDriver,
    /// Advance one step per editor frame.
    pub playing: bool,
    /// Draw the `locomotion` gizmo overlay in the panel.
    pub gizmos: bool,
    /// The walker (`None` before the first reset, or after a failed one).
    pub state: Option<GaitState>,
    /// The run's metrics.
    pub metrics: LocomotionMetrics,
    /// The last error (a failed start or step); a reset clears it.
    pub error: Option<LocomotionError>,
    /// `GaitState::refused` grew on the last step (the overlay's refused marker).
    refused_now: bool,
    /// The egui frame that last stepped while playing (egui may run a panel twice per frame).
    last_frame: Option<u64>,
}

impl Default for LocomotionTestBed {
    fn default() -> Self {
        let params = default_gait();
        Self {
            source: LocomotionSource::Default,
            metrics: LocomotionMetrics::new(params.legs()),
            params,
            ground: GroundPreset::Flat,
            driver: LocomotionDriver::default(),
            playing: false,
            gizmos: true,
            state: None,
            error: None,
            refused_now: false,
            last_frame: None,
        }
    }
}

/// The built-in walker: the six-leg tripod with the agent guide's fixture values (the same
/// settings `anim.gait_simulate` steps when it gets no `params`).
#[must_use]
pub fn default_gait() -> GaitParams {
    GaitParams {
        forward_speed: 2.4,
        reverse_speed: 1.1,
        strafe_speed: 0.8,
        acceleration: 3.0,
        step_length: 0.6,
        clearance: 0.7,
        swing_lift: 0.18,
        max_slope: 0.9,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array(
            [
                [-0.4, 0.5],
                [0.4, 0.5],
                [-0.5, 0.0],
                [0.5, 0.0],
                [-0.4, -0.5],
                [0.4, -0.5],
            ],
            [0.0; 2],
        ),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 2.0,
        foothold_candidates: 16,
        foothold_spacing: 0.25,
        max_reach: [1.2; MAX_LEGS],
        hang_drop: 0.3,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    }
}

impl LocomotionTestBed {
    /// A test bed for `params` on `ground` under `driver`, reset to rest.
    #[must_use]
    pub fn new(params: GaitParams, ground: GroundPreset, driver: LocomotionDriver) -> Self {
        let mut bed = Self {
            params,
            ground,
            driver,
            ..Self::default()
        };
        bed.reset();
        bed
    }

    /// Simulated time so far, seconds.
    #[must_use]
    pub fn time(&self) -> f32 {
        self.metrics.steps as f32 * PREVIEW_DT
    }

    /// The body the driver gives at `time`, on the ground layer near `floor`.
    fn body(&self, time: f32, floor: f32) -> GaitBody {
        let (xz, yaw, v) = self.driver.at(time);
        let y = self.ground.height_near(xz[0], xz[1], floor) + self.params.clearance;
        GaitBody {
            position: [xz[0], y, xz[1]],
            yaw,
            velocity: [v[0], 0.0, v[1]],
        }
    }

    /// Plants every foot at neutral under the driver's start and clears the metrics.
    pub fn reset(&mut self) {
        self.metrics = LocomotionMetrics::new(self.params.legs());
        self.error = None;
        self.refused_now = false;
        let body = GaitBody {
            velocity: [0.0; 3],
            ..self.body(0.0, 0.0)
        };
        match GaitState::at_rest_driven(&self.params, body, self.ground) {
            Ok(state) => self.state = Some(state),
            Err(error) => {
                self.state = None;
                self.error = Some(LocomotionError { error, step: None });
            }
        }
    }

    /// Advances one fixed step ([`PREVIEW_DT`]) and folds it into the metrics. Allocates
    /// nothing.
    ///
    /// # Errors
    ///
    /// The stepper's error (also kept in [`Self::error`]; later steps do nothing until a
    /// reset).
    pub fn step(&mut self) -> Result<(), LocomotionError> {
        let _span = tracing::info_span!(target: "regia_editor", "locomotion_test_bed_step").entered();
        if let Some(error) = self.error {
            return Err(error);
        }
        let Some(mut state) = self.state else {
            let error = LocomotionError {
                error: GaitError::State,
                step: None,
            };
            self.error = Some(error);
            return Err(error);
        };
        let before = state;
        let time = (self.metrics.steps + 1) as f32 * PREVIEW_DT;
        let floor = state.position[1] - self.params.clearance;
        let body = self.body(time, floor);
        if let Err(error) = step_gait_driven(&self.params, &mut state, body, PREVIEW_DT, self.ground) {
            let error = LocomotionError {
                error,
                step: Some(self.metrics.steps),
            };
            self.error = Some(error);
            return Err(error);
        }
        self.refused_now = state.refused > before.refused;
        self.metrics.observe(&before, &state);
        self.state = Some(state);
        Ok(())
    }

    /// Resets, then runs `seconds` of fixed steps.
    ///
    /// # Errors
    ///
    /// The first stepper error.
    pub fn run(&mut self, seconds: f32) -> Result<LocomotionMetrics, LocomotionError> {
        let _span = tracing::info_span!(target: "regia_editor", "locomotion_test_bed_run").entered();
        self.reset();
        if let Some(error) = self.error {
            return Err(error);
        }
        for _ in 0..steps_for(seconds) {
            self.step()?;
        }
        Ok(self.metrics)
    }

    /// The overlay segments of the current state: the `locomotion` gizmo group's gait drawing
    /// ([`draw_gait`]) into `buffer` (cleared first). Empty before a reset.
    pub fn draw_overlay(&self, buffer: &mut GizmoBuffer<LocomotionGizmos>, colors: &LocomotionGizmoColors) {
        buffer.clear();
        if let Some(state) = &self.state {
            let mut stats = LocomotionGizmoStats::default();
            draw_gait(
                buffer,
                &self.params,
                state,
                None,
                self.refused_now,
                colors,
                &mut stats,
            );
        }
    }
}

/// Fixed steps in `seconds` (rounded to the nearest step).
#[must_use]
pub fn steps_for(seconds: f32) -> u64 {
    (seconds.max(0.0) / PREVIEW_DT).round() as u64
}

// ---------------------------------------------------------------------------------------------
// Settings, ranges and undoable component edits

/// A documented setting: reflected field name, label, unit, inclusive range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SettingRange {
    /// Field name (`GaitParams` or `FootPlantSettings`).
    pub field: &'static str,
    /// What it does.
    pub label: &'static str,
    /// Unit shown next to the value.
    pub unit: &'static str,
    /// Smallest value the editor accepts.
    pub min: f32,
    /// Largest value the editor accepts.
    pub max: f32,
}

const fn range(
    field: &'static str,
    label: &'static str,
    unit: &'static str,
    min: f32,
    max: f32,
) -> SettingRange {
    SettingRange {
        field,
        label,
        unit,
        min,
        max,
    }
}

/// The `GaitRig.params` settings the Inspector section edits, with their ranges.
pub const GAIT_SETTING_RANGES: &[SettingRange] = &[
    range("forward_speed", "Forward speed", "m/s", 0.0, 20.0),
    range("reverse_speed", "Reverse speed", "m/s", 0.0, 20.0),
    range("strafe_speed", "Strafe speed", "m/s", 0.0, 20.0),
    range("acceleration", "Acceleration", "m/s²", 0.01, 100.0),
    range("step_length", "Step length", "m", 0.01, 5.0),
    range("clearance", "Body clearance", "m", 0.0, 5.0),
    range("swing_lift", "Swing lift", "m", 0.0, 2.0),
    range("max_slope", "Max slope (rise/run)", "", 0.0, 5.0),
    range("idle_speed", "Idle speed", "m/s", 0.0, 2.0),
    range("max_yaw_rate", "Max yaw rate", "rad/s", 0.0, 12.6),
    range("settle_distance", "Settle distance", "m", 0.0, 2.0),
    range("settle_period", "Settle period", "s", 0.01, 5.0),
    range("turn_step_distance", "Turn step distance", "m", 0.0, 2.0),
    range(
        "turn_step_angle",
        "Turn step angle",
        "rad",
        0.0,
        core::f32::consts::PI,
    ),
    range("ground_band", "Ground band", "m", 0.05, 50.0),
    range("foothold_spacing", "Foothold spacing", "m", 0.01, 5.0),
    range("hang_drop", "Hang drop", "m", 0.0, 5.0),
];

/// The `FootPlacement.settings` scalars the Inspector section edits, with their ranges.
pub const FOOT_SETTING_RANGES: &[SettingRange] = &[
    range("max_ground_offset", "Max ground offset", "m", 0.0, 2.0),
    range("max_pelvis_drop", "Max pelvis drop", "m", 0.0, 2.0),
    range("max_tilt", "Max tilt", "rad", 0.0, 1.5),
    range("plant_lift", "Plant lift", "m", 0.0, 0.5),
    range("swing_lift", "Swing lift", "m", 0.0, 0.5),
    range("max_extension", "Max extension", "", 0.5, 1.0),
    range("deadband", "Dead band", "m", 0.0, 0.2),
    range("max_slew", "Max slew", "m/s", 0.0, 50.0),
    range("lock_max_drift", "Lock max drift", "m", 0.0, 2.0),
    range("lock_release_time", "Lock release time", "s", 0.0, 2.0),
];

/// The settings range named `field` in `table`.
#[must_use]
pub fn setting_range(table: &[SettingRange], field: &str) -> Option<SettingRange> {
    table.iter().copied().find(|r| r.field == field)
}

fn gait_scalar<'a>(p: &'a mut GaitParams, key: &str) -> Option<&'a mut f32> {
    Some(match key {
        "forward_speed" => &mut p.forward_speed,
        "reverse_speed" => &mut p.reverse_speed,
        "strafe_speed" => &mut p.strafe_speed,
        "acceleration" => &mut p.acceleration,
        "step_length" => &mut p.step_length,
        "clearance" => &mut p.clearance,
        "swing_lift" => &mut p.swing_lift,
        "max_slope" => &mut p.max_slope,
        "idle_speed" => &mut p.idle_speed,
        "max_yaw_rate" => &mut p.max_yaw_rate,
        "settle_distance" => &mut p.settle_distance,
        "settle_period" => &mut p.settle_period,
        "turn_step_distance" => &mut p.turn_step_distance,
        "turn_step_angle" => &mut p.turn_step_angle,
        "ground_band" => &mut p.ground_band,
        "foothold_spacing" => &mut p.foothold_spacing,
        "hang_drop" => &mut p.hang_drop,
        _ => return None,
    })
}

/// Edits component `T` of `entity` as one undo step: a whole-component `SetProperty` (not
/// coalesced), the same command an Inspector field edit pushes. Returns `Ok(false)` when `edit`
/// changed nothing (no undo entry).
///
/// # Errors
///
/// The entity or component is missing, `edit` refused, or the type is not registered.
pub fn edit_component<T>(
    world: &mut World,
    entity: Entity,
    edit: impl FnOnce(&mut T) -> Result<(), String>,
) -> Result<bool, String>
where
    T: Component + Reflect + TypePath + Clone,
{
    let short = T::short_type_path();
    let Some(current) = world.get_entity(entity).ok().and_then(|e| e.get::<T>()).cloned() else {
        return Err(format!("entity {} has no {short}", entity.to_bits()));
    };
    let mut next = current.clone();
    edit(&mut next)?;
    if current
        .reflect_partial_eq(next.as_partial_reflect())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let new: Box<dyn PartialReflect> = Box::new(next);
    let mut command =
        SetProperty::capture(world, entity, T::type_path(), "", new).map_err(|e| e.to_string())?;
    command.coalesce = false;
    command.apply(world);
    crate::undo_agent::push_discrete(world, Box::new(command));
    Ok(true)
}

/// Sets one `GaitRig.params` scalar (a [`GAIT_SETTING_RANGES`] field), refusing values outside
/// its range or settings the stepper refuses; one undo step.
///
/// # Errors
///
/// An unknown field, a value out of range, or settings that fail `GaitParams::validate`.
pub fn set_gait_setting(world: &mut World, entity: Entity, field: &str, value: f32) -> Result<bool, String> {
    let range = setting_range(GAIT_SETTING_RANGES, field)
        .ok_or_else(|| format!("GaitRig has no setting `{field}`"))?;
    if !(value.is_finite() && (range.min..=range.max).contains(&value)) {
        return Err(format!(
            "`{field}` must be in {}..={} {}",
            range.min, range.max, range.unit
        ));
    }
    edit_component::<GaitRig>(world, entity, |rig| {
        if let Some(slot) = gait_scalar(&mut rig.params, field) {
            *slot = value;
        }
        rig.params.validate().map_err(|e| e.to_string())
    })
}

/// Replaces the rig's patterns by one preset for its leg count ([`GaitPatternSpec::presets`];
/// ripple and wave take the rig's leg count); one undo step.
///
/// # Errors
///
/// `gait_pattern` when the preset does not drive the rig's number of legs.
pub fn apply_pattern_preset(
    world: &mut World,
    entity: Entity,
    preset: &GaitPatternSpec,
) -> Result<bool, String> {
    edit_component::<GaitRig>(world, entity, |rig| {
        let legs = u8::try_from(rig.legs.len()).unwrap_or(u8::MAX);
        let spec = match preset {
            GaitPatternSpec::Ripple(_) => GaitPatternSpec::Ripple(legs),
            GaitPatternSpec::Wave(_) => GaitPatternSpec::Wave(legs),
            other => other.clone(),
        };
        let pattern = spec.build().map_err(|e| e.to_string())?;
        if pattern.leg_count() != rig.legs.len() {
            return Err(format!(
                "gait_pattern: {spec:?} drives {} legs; the rig has {}",
                pattern.leg_count(),
                rig.legs.len()
            ));
        }
        rig.params.patterns = GaitPatternSet::single(pattern);
        rig.params.validate().map_err(|e| e.to_string())
    })
}

/// Sets leg `index`'s chain (root and tip joint names); `index == legs.len()` appends a leg.
/// One undo step.
///
/// # Errors
///
/// An index past the end, more than 16 legs, or an empty joint name.
pub fn set_gait_leg(
    world: &mut World,
    entity: Entity,
    index: usize,
    root: &str,
    tip: &str,
) -> Result<bool, String> {
    if root.trim().is_empty() || tip.trim().is_empty() {
        return Err("a leg needs a root and a tip joint".into());
    }
    edit_component::<GaitRig>(world, entity, |rig| {
        match index.cmp(&rig.legs.len()) {
            std::cmp::Ordering::Less => {
                rig.legs[index].root = root.to_owned();
                rig.legs[index].tip = tip.to_owned();
            }
            std::cmp::Ordering::Equal if index < MAX_LEGS => rig.legs.push(GaitLeg::new(root, tip)),
            _ => return Err(format!("leg {index} is past the end (at most {MAX_LEGS} legs)")),
        }
        Ok(())
    })
}

/// Removes leg `index`; one undo step.
///
/// # Errors
///
/// An index past the end.
pub fn remove_gait_leg(world: &mut World, entity: Entity, index: usize) -> Result<bool, String> {
    edit_component::<GaitRig>(world, entity, |rig| {
        if index >= rig.legs.len() {
            return Err(format!("leg {index} is past the end"));
        }
        rig.legs.remove(index);
        Ok(())
    })
}

/// Sets one `FootPlacement.settings` scalar (a [`FOOT_SETTING_RANGES`] field); one undo step.
///
/// # Errors
///
/// An unknown field or a value out of range.
pub fn set_foot_setting(world: &mut World, entity: Entity, field: &str, value: f32) -> Result<bool, String> {
    let range = setting_range(FOOT_SETTING_RANGES, field)
        .ok_or_else(|| format!("FootPlacement has no setting `{field}`"))?;
    if !(value.is_finite() && (range.min..=range.max).contains(&value)) {
        return Err(format!(
            "`{field}` must be in {}..={} {}",
            range.min, range.max, range.unit
        ));
    }
    edit_component::<FootPlacement>(world, entity, |foot| {
        let s = &mut foot.settings;
        let slot = match field {
            "max_ground_offset" => &mut s.max_ground_offset,
            "max_pelvis_drop" => &mut s.max_pelvis_drop,
            "max_tilt" => &mut s.max_tilt,
            "plant_lift" => &mut s.plant_lift,
            "swing_lift" => &mut s.swing_lift,
            "max_extension" => &mut s.max_extension,
            "deadband" => &mut s.deadband,
            "max_slew" => &mut s.max_slew,
            "lock_max_drift" => &mut s.lock_max_drift,
            _ => &mut s.lock_release_time,
        };
        *slot = value;
        Ok(())
    })
}

/// Sets foot leg `index` (hip, knee, ankle, optional toe); `index == legs.len()` appends one.
/// One undo step.
///
/// # Errors
///
/// An index past the end or an empty required joint name.
pub fn set_foot_leg(
    world: &mut World,
    entity: Entity,
    index: usize,
    leg: FootLegDesc,
) -> Result<bool, String> {
    if leg.hip.trim().is_empty() || leg.knee.trim().is_empty() || leg.ankle.trim().is_empty() {
        return Err("a foot leg needs hip, knee and ankle joints".into());
    }
    edit_component::<FootPlacement>(world, entity, |foot| {
        match index.cmp(&foot.legs.len()) {
            std::cmp::Ordering::Less => foot.legs[index] = leg,
            std::cmp::Ordering::Equal => foot.legs.push(leg),
            std::cmp::Ordering::Greater => return Err(format!("leg {index} is past the end")),
        }
        Ok(())
    })
}

/// The rig's gait settings as a `.gait.ron` document (the format's own writer).
#[must_use]
pub fn gait_file_of(rig: &GaitRig) -> GaitFile {
    GaitFile::from_params(&rig.params, &rig.legs, rig.body.as_deref())
}

/// Writes `entity`'s `GaitRig` settings to `path` as `.gait.ron` (format `gait` v1), atomically
/// (a temporary sibling, then a rename). Returns the bytes written.
///
/// # Errors
///
/// `gait_*` codes from the writer, or the IO failure.
pub fn save_gait_ron(world: &World, entity: Entity, path: &Path) -> Result<usize, GaitAssetError> {
    let rig = world
        .get_entity(entity)
        .ok()
        .and_then(|e| e.get::<GaitRig>())
        .ok_or_else(|| GaitAssetError::Io(format!("entity {} has no GaitRig", entity.to_bits())))?;
    let file = gait_file_of(rig);
    // Refuse settings the loader would refuse: the saved file must load.
    let text = GaitAsset::from_file(file)?.to_ron()?;
    crate::atomic_write::write_atomic(path, text.as_bytes())
        .map_err(|e| GaitAssetError::Io(format!("{}: {e}", path.display())))?;
    Ok(text.len())
}

/// Loads a `.gait.ron` into `entity`'s `GaitRig` (params, legs and body), one undo step.
///
/// # Errors
///
/// The loader's `gait_*` codes, or `gait_io` when the entity has no `GaitRig`.
pub fn load_gait_into(world: &mut World, entity: Entity, path: &Path) -> Result<bool, GaitAssetError> {
    let asset = regia_animation::gait_asset::load_gait_file(path)?;
    let legs = asset.legs();
    edit_component::<GaitRig>(world, entity, |rig| {
        rig.params = asset.params;
        rig.legs = legs;
        rig.body.clone_from(&asset.file.body);
        Ok(())
    })
    .map_err(GaitAssetError::Io)
}

/// The test bed settings of `entity`'s `GaitRig`: its params, with hip offsets measured from
/// the bind pose (driven convention) when the rig measures them and has a skeleton.
///
/// # Errors
///
/// `no_such_entity`, `no_gait_rig`, or the stepper's `gait_settings` / `gait_pattern`.
pub fn entity_gait(world: &World, bits: u64) -> Result<GaitParams, RpcError> {
    let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
        RpcError::new(
            NOT_FOUND,
            "no_such_entity",
            format!("no entity {bits}"),
            "list entities with scene.query_entities",
        )
    })?;
    let rig = world.get::<GaitRig>(entity).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "no_gait_rig",
            format!("entity {bits} has no GaitRig"),
            "add a GaitRig (book/agents/procedural-gait.md) or pass `asset`",
        )
    })?;
    let mut params = rig.params;
    if rig.auto_hips
        && let Some(mesh) = world.get::<SkinnedMesh>(entity)
        && let Ok(hips) = GaitParams::hip_offsets_from_skeleton(&mesh.skeleton, &rig.legs, GaitMode::Driven)
    {
        params.hip_offsets = hips;
    }
    params
        .validate()
        .map_err(|e| gait_rpc_error(e, "the rig's GaitRig.params"))?;
    Ok(params)
}

/// An agent error carrying a stepper error's code.
#[must_use]
pub fn gait_rpc_error(error: GaitError, context: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        gait_error_code(error),
        format!("{context}: {error}"),
        "see book/agents/animation-editor.md (Locomotion test bed) and book/agents/procedural-gait.md",
    )
}

// ---------------------------------------------------------------------------------------------
// UI

fn color32(c: regia_core::math::Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        crate::inspector::linear_to_srgb_u8(c.r),
        crate::inspector::linear_to_srgb_u8(c.g),
        crate::inspector::linear_to_srgb_u8(c.b),
        (c.a.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// A fixed three-quarter view around `center`: world meters to panel points.
struct View {
    center: Vec3,
    rect: egui::Rect,
    scale: f32,
}

impl View {
    fn project(&self, p: Vec3) -> egui::Pos2 {
        // Yaw 35 degrees, pitch 30 degrees, orthographic.
        let (sy, cy) = (0.573_576_4_f32, 0.819_152_f32);
        let (sp, cp) = (0.5_f32, 0.866_025_4_f32);
        let d = p - self.center;
        let x = d.x * cy - d.z * sy;
        let depth = d.x * sy + d.z * cy;
        let y = d.y * cp - depth * sp;
        self.rect.center() + egui::vec2(x * self.scale, -y * self.scale)
    }
}

/// Paints the ground around the walker, the body and (when on) the gizmo overlay.
fn paint_test_bed(ui: &mut egui::Ui, bed: &LocomotionTestBed, size: egui::Vec2) {
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    let Some(state) = &bed.state else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Press Reset to plant the walker",
            egui::FontId::proportional(13.0),
            ui.visuals().weak_text_color(),
        );
        return;
    };
    let center = Vec3::from_array(state.position);
    let view = View {
        center,
        rect,
        scale: rect.width().min(rect.height()) / 5.0,
    };
    // Ground: a 2.5 m grid of layer heights around the body.
    let grid = egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.5));
    let floor = state.position[1] - bed.params.clearance;
    let n = 10;
    let step = 0.5;
    let origin = [(center.x / step).round() * step, (center.z / step).round() * step];
    let at = |i: i32, j: i32| {
        let x = origin[0] + (i - n / 2) as f32 * step;
        let z = origin[1] + (j - n / 2) as f32 * step;
        Vec3::new(x, bed.ground.height_near(x, z, floor), z)
    };
    for i in 0..=n {
        for j in 0..n {
            painter.line_segment([view.project(at(i, j)), view.project(at(i, j + 1))], grid);
            painter.line_segment([view.project(at(j, i)), view.project(at(j + 1, i))], grid);
        }
    }
    if let GroundPreset::Tunnel { depth } = bed.ground {
        // The upper layer's outline, so the two layers read apart.
        let roof = egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.3));
        let corners = [(-2.5, -2.5), (2.5, -2.5), (2.5, 2.5), (-2.5, 2.5)]
            .map(|(dx, dz)| view.project(Vec3::new(center.x + dx, depth, center.z + dz)));
        for (a, b) in corners.iter().zip(corners.iter().cycle().skip(1)) {
            painter.line_segment([*a, *b], roof);
        }
    }
    let text = ui.visuals().text_color();
    painter.circle_stroke(view.project(center), 4.0, egui::Stroke::new(2.0, text));
    if bed.gizmos {
        let colors = LocomotionGizmoColors::default();
        let mut buffer = GizmoBuffer::<LocomotionGizmos>::new();
        bed.draw_overlay(&mut buffer, &colors);
        let (lines, _) = buffer.list_positions.as_chunks::<2>();
        let (line_colors, _) = buffer.list_colors.as_chunks::<2>();
        for (pair, color) in lines.iter().zip(line_colors) {
            painter.line_segment(
                [view.project(pair[0]), view.project(pair[1])],
                egui::Stroke::new(1.5, color32(color[0])),
            );
        }
        for (pair, color) in buffer
            .strip_positions
            .windows(2)
            .zip(buffer.strip_colors.windows(2))
        {
            if pair[0].is_finite() && pair[1].is_finite() {
                painter.line_segment(
                    [view.project(pair[0]), view.project(pair[1])],
                    egui::Stroke::new(1.5, color32(color[0])),
                );
            }
        }
    }
}

/// The metrics grid of a run.
fn metrics_ui(ui: &mut egui::Ui, metrics: &LocomotionMetrics) {
    egui::Grid::new("locomotion-metrics")
        .num_columns(2)
        .show(ui, |ui| {
            let mut row = |label: &str, value: String| {
                ui.label(label);
                ui.monospace(value);
                ui.end_row();
            };
            row(
                "Time",
                format!(
                    "{:.2} s ({} steps)",
                    metrics.steps as f32 * PREVIEW_DT,
                    metrics.steps
                ),
            );
            row("Max slide", format!("{:.4} m", metrics.max_slide));
            row("Max foot jump", format!("{:.4} m", metrics.max_foot_jump));
            row("Forced steps", metrics.forced_steps.to_string());
            row("Refused", metrics.refused.to_string());
            row("Min planted", metrics.min_planted.to_string());
            let margin = |m: f32| {
                if m.is_finite() {
                    format!("{m:.3} m")
                } else {
                    "–".to_owned()
                }
            };
            row("Stability margin", margin(metrics.stability_margin));
            row("Min stability margin", margin(metrics.min_stability_margin));
        });
}

/// The ground preset controls; returns whether anything changed.
fn ground_ui(ui: &mut egui::Ui, ground: &mut GroundPreset) -> bool {
    let mut changed = false;
    egui::ComboBox::from_label("Ground")
        .selected_text(ground.name())
        .show_ui(ui, |ui| {
            for preset in GroundPreset::presets() {
                if ui
                    .selectable_label(ground.name() == preset.name(), preset.name())
                    .clicked()
                {
                    changed |= *ground != preset;
                    *ground = preset;
                }
            }
        });
    let mut drag = |ui: &mut egui::Ui, label: &str, v: &mut f32, lo: f32, hi: f32, speed: f64| {
        ui.horizontal(|ui| {
            ui.label(label);
            changed |= ui
                .add(egui::DragValue::new(v).range(lo..=hi).speed(speed))
                .changed();
        });
    };
    match ground {
        GroundPreset::Flat => {}
        GroundPreset::Slope { degrees } => drag(ui, "Incline (°)", degrees, 0.0, 60.0, 0.5),
        GroundPreset::Stairs { rise, run } => {
            drag(ui, "Rise (m)", rise, 0.0, 1.0, 0.01);
            drag(ui, "Run (m)", run, 0.05, 10.0, 0.01);
        }
        GroundPreset::Rubble {
            amplitude,
            cell,
            seed,
        } => {
            drag(ui, "Amplitude (m)", amplitude, 0.0, 2.0, 0.01);
            drag(ui, "Cell (m)", cell, 0.05, 10.0, 0.01);
            ui.horizontal(|ui| {
                ui.label("Seed");
                changed |= ui.add(egui::DragValue::new(seed)).changed();
            });
        }
        GroundPreset::Tunnel { depth } => drag(ui, "Upper layer (m)", depth, 0.5, 50.0, 0.1),
    }
    changed
}

/// The locomotion test bed panel (a section of the animation preview; also
/// `editor.capture_panel {panel: "anim_locomotion"}`).
pub fn anim_locomotion_panel(ui: &mut egui::Ui, world: &mut World) {
    let frame = ui.ctx().cumulative_frame_nr();
    let Some(mut editor) = world.get_resource_mut::<super::AnimEditor>() else {
        ui.weak("The animation editor is not open.");
        return;
    };
    let bed = &mut editor.locomotion;
    test_bed_ui(ui, bed, frame);
}

/// The test bed's controls, metrics and view.
pub fn test_bed_ui(ui: &mut egui::Ui, bed: &mut LocomotionTestBed, frame: u64) {
    ui.horizontal_wrapped(|ui| {
        ui.strong("Locomotion test bed");
        ui.weak(match &bed.source {
            LocomotionSource::Default => "built-in tripod".to_owned(),
            LocomotionSource::Entity(bits) => format!("GaitRig of entity {bits}"),
            LocomotionSource::Asset(path) => path.clone(),
        });
    });
    let mut reset = ground_ui(ui, &mut bed.ground);
    ui.horizontal(|ui| {
        ui.label("Speed (m/s)");
        reset |= ui
            .add(
                egui::DragValue::new(&mut bed.driver.speed)
                    .range(0.0..=MAX_DRIVER_SPEED)
                    .speed(0.05),
            )
            .changed();
        ui.label("Turn (rad/s)");
        reset |= ui
            .add(
                egui::DragValue::new(&mut bed.driver.turn_rate)
                    .range(-MAX_DRIVER_TURN_RATE..=MAX_DRIVER_TURN_RATE)
                    .speed(0.01),
            )
            .changed();
    });
    ui.horizontal(|ui| {
        let play = if bed.playing { "Pause" } else { "Play" };
        if ui.button(play).clicked() {
            bed.playing = !bed.playing;
        }
        if ui.button("Step").clicked() {
            bed.playing = false;
            if bed.state.is_none() && bed.error.is_none() {
                bed.reset();
            }
            let _ = bed.step();
        }
        if ui.button("Reset").clicked() {
            reset = true;
        }
        ui.checkbox(&mut bed.gizmos, "Gizmos");
    });
    if reset {
        bed.reset();
    }
    if bed.playing && bed.last_frame != Some(frame) {
        bed.last_frame = Some(frame);
        if bed.state.is_none() && bed.error.is_none() {
            bed.reset();
        }
        if bed.step().is_err() {
            bed.playing = false;
        }
        ui.ctx().request_repaint();
    }
    if let Some(error) = &bed.error {
        ui.colored_label(ui.visuals().error_fg_color, format!("{}: {error}", error.code()));
    }
    metrics_ui(ui, &bed.metrics);
    let width = ui.available_width().max(160.0);
    paint_test_bed(ui, bed, egui::vec2(width, (width * 0.65).clamp(120.0, 420.0)));
}

/// A joint picker over `joints`; returns the picked name when it changed.
fn joint_picker(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    current: &str,
    joints: &[String],
) -> Option<String> {
    let mut picked = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(if current.is_empty() {
            "(pick a joint)"
        } else {
            current
        })
        .width(120.0)
        .show_ui(ui, |ui| {
            for name in joints {
                if ui.selectable_label(name == current, name).clicked() && name != current {
                    picked = Some(name.clone());
                }
            }
        });
    picked
}

fn joint_names(world: &World, entity: Entity) -> Vec<String> {
    world
        .get::<SkinnedMesh>(entity)
        .map(|mesh| mesh.skeleton.joint_names.clone())
        .unwrap_or_default()
}

/// A component edit a section collected while drawing, applied after the draw.
type PendingEdit = Box<dyn FnOnce(&mut World) -> Result<bool, String>>;

fn report(world: &mut World, result: Result<bool, String>) {
    if let Err(message) = result
        && let Some(mut editor) = world.get_resource_mut::<super::AnimEditor>()
    {
        editor.toast = message;
    }
}

/// The Inspector section below a `GaitRig`'s reflected fields: ranged settings, leg chain
/// pickers, pattern presets, the gait asset picker and "Preview in the test bed".
pub fn gait_rig_section(ui: &mut egui::Ui, world: &mut World, entity: Entity) {
    let Some(rig) = world.get::<GaitRig>(entity).cloned() else {
        return;
    };
    let joints = joint_names(world, entity);
    ui.strong("Gait authoring");
    let mut edits: Vec<PendingEdit> = Vec::new();
    egui::CollapsingHeader::new("Settings (documented ranges)")
        .id_salt(("gait-settings", entity))
        .show(ui, |ui| {
            for r in GAIT_SETTING_RANGES {
                let mut params = rig.params;
                let Some(slot) = gait_scalar(&mut params, r.field) else {
                    continue;
                };
                let original = *slot;
                let mut value = original;
                ui.horizontal(|ui| {
                    if !(r.min..=r.max).contains(&original) {
                        // A value set in code outside the editor's range (an unbounded band):
                        // shown, edited only in the reflected field above.
                        ui.label(format!(
                            "{} = {original} (outside {}..={})",
                            r.label, r.min, r.max
                        ));
                        return;
                    }
                    let response = ui
                        .add(egui::DragValue::new(&mut value).range(r.min..=r.max).speed(0.01))
                        .on_hover_text(format!("{}..={} {}", r.min, r.max, r.unit));
                    ui.label(format!("{} ({})", r.label, r.unit));
                    if response.changed() && value != original {
                        let field = r.field;
                        edits.push(Box::new(move |w: &mut World| {
                            set_gait_setting(w, entity, field, value)
                        }));
                    }
                });
            }
        });
    egui::CollapsingHeader::new(format!("Legs ({})", rig.legs.len()))
        .id_salt(("gait-legs", entity))
        .show(ui, |ui| {
            if joints.is_empty() {
                ui.weak("No skeleton on this entity: joint names are typed in the fields above.");
            }
            for (i, leg) in rig.legs.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{i}"));
                    if let Some(root) = joint_picker(ui, ("gait-root", entity, i), &leg.root, &joints) {
                        let tip = leg.tip.clone();
                        edits.push(Box::new(move |w: &mut World| {
                            set_gait_leg(w, entity, i, &root, &tip)
                        }));
                    }
                    ui.label("→");
                    if let Some(tip) = joint_picker(ui, ("gait-tip", entity, i), &leg.tip, &joints) {
                        let root = leg.root.clone();
                        edits.push(Box::new(move |w: &mut World| {
                            set_gait_leg(w, entity, i, &root, &tip)
                        }));
                    }
                    if ui.small_button("✕").on_hover_text("Remove this leg").clicked() {
                        edits.push(Box::new(move |w: &mut World| remove_gait_leg(w, entity, i)));
                    }
                });
            }
            if rig.legs.len() < MAX_LEGS && !joints.is_empty() && ui.button("Add leg").clicked() {
                let first = joints[0].clone();
                let last = joints[joints.len() - 1].clone();
                let at = rig.legs.len();
                edits.push(Box::new(move |w: &mut World| {
                    set_gait_leg(w, entity, at, &first, &last)
                }));
            }
        });
    ui.horizontal_wrapped(|ui| {
        ui.label("Pattern preset");
        for preset in GaitPatternSpec::presets() {
            let name = match &preset {
                GaitPatternSpec::Ripple(_) => "Ripple".to_owned(),
                GaitPatternSpec::Wave(_) => "Wave".to_owned(),
                other => format!("{other:?}"),
            };
            if ui.small_button(name).clicked() {
                edits.push(Box::new(move |w: &mut World| {
                    apply_pattern_preset(w, entity, &preset)
                }));
            }
        }
    });
    let path_id = egui::Id::new(("gait-asset-path", entity));
    let mut path: String = ui.data_mut(|d| d.get_temp(path_id)).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("Gait asset");
        ui.add(
            egui::TextEdit::singleline(&mut path)
                .hint_text("assets/units/walker.gait.ron")
                .desired_width(180.0),
        );
        let file = path.clone();
        if ui
            .button("Load")
            .on_hover_text("Copy the file's settings into this rig (undoable)")
            .clicked()
        {
            edits.push(Box::new(move |w: &mut World| {
                let full = super::graph_panel::agent_path(w, "asset", &file).map_err(|e| e.message)?;
                load_gait_into(w, entity, &full).map_err(|e| e.to_string())
            }));
        }
        let file = path.clone();
        if ui.button("Save .gait.ron").clicked() {
            edits.push(Box::new(move |w: &mut World| {
                let full = super::graph_panel::agent_path(w, "asset", &file).map_err(|e| e.message)?;
                save_gait_ron(w, entity, &full)
                    .map(|_| false)
                    .map_err(|e| e.to_string())
            }));
        }
    });
    ui.data_mut(|d| d.insert_temp(path_id, path));
    if ui.button("Preview in the test bed").clicked() {
        edits.push(Box::new(move |w: &mut World| {
            let params = entity_gait(w, entity.to_bits()).map_err(|e| e.message)?;
            if let Some(mut editor) = w.get_resource_mut::<super::AnimEditor>() {
                let bed = &mut editor.locomotion;
                bed.source = LocomotionSource::Entity(entity.to_bits());
                bed.params = params;
                bed.reset();
            }
            Ok(false)
        }));
    }
    for edit in edits {
        let result = edit(world);
        report(world, result);
    }
}

/// The Inspector section below a `FootPlacement`'s reflected fields: ranged planter settings
/// and the leg chain pickers (hip, knee, ankle, toe).
pub fn foot_placement_section(ui: &mut egui::Ui, world: &mut World, entity: Entity) {
    let Some(foot) = world.get::<FootPlacement>(entity).cloned() else {
        return;
    };
    let joints = joint_names(world, entity);
    ui.strong("Foot placement authoring");
    let mut edits: Vec<PendingEdit> = Vec::new();
    egui::CollapsingHeader::new("Planter settings (documented ranges)")
        .id_salt(("foot-settings", entity))
        .show(ui, |ui| {
            let values = [
                foot.settings.max_ground_offset,
                foot.settings.max_pelvis_drop,
                foot.settings.max_tilt,
                foot.settings.plant_lift,
                foot.settings.swing_lift,
                foot.settings.max_extension,
                foot.settings.deadband,
                foot.settings.max_slew,
                foot.settings.lock_max_drift,
                foot.settings.lock_release_time,
            ];
            for (r, original) in FOOT_SETTING_RANGES.iter().zip(values) {
                let mut value = original;
                ui.horizontal(|ui| {
                    if !(r.min..=r.max).contains(&original) {
                        ui.label(format!(
                            "{} = {original} (outside {}..={})",
                            r.label, r.min, r.max
                        ));
                        return;
                    }
                    let response = ui
                        .add(egui::DragValue::new(&mut value).range(r.min..=r.max).speed(0.005))
                        .on_hover_text(format!("{}..={} {}", r.min, r.max, r.unit));
                    ui.label(format!("{} ({})", r.label, r.unit));
                    if response.changed() && value != original {
                        let field = r.field;
                        edits.push(Box::new(move |w: &mut World| {
                            set_foot_setting(w, entity, field, value)
                        }));
                    }
                });
            }
        });
    egui::CollapsingHeader::new(format!("Legs ({})", foot.legs.len()))
        .id_salt(("foot-legs", entity))
        .show(ui, |ui| {
            for (i, leg) in foot.legs.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{i}"));
                    for (part, current) in [("hip", &leg.hip), ("knee", &leg.knee), ("ankle", &leg.ankle)] {
                        if let Some(name) = joint_picker(ui, ("foot", part, entity, i), current, &joints) {
                            let mut next = leg.clone();
                            match part {
                                "hip" => next.hip = name,
                                "knee" => next.knee = name,
                                _ => next.ankle = name,
                            }
                            edits.push(Box::new(move |w: &mut World| set_foot_leg(w, entity, i, next)));
                        }
                    }
                });
            }
        });
    for edit in edits {
        let result = edit(world);
        report(world, result);
    }
}
