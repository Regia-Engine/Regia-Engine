// SPDX-License-Identifier: MPL-2.0
//! The `hover_params_step` target (lane mp-hover-robust-inputs): a hover hull (`HoverParams`, user
//! content: mods, map files) plus up to 64 ticks of input, floor and `dt`, as RON. The loader flies
//! the hull as written (it must never panic or hang, whatever the numbers) and then as
//! `HoverParams::sanitized` within the documented domain (book `agents/det-hover.md`, "Bad input"),
//! where every state word must stay finite; then it writes the run back.

use regia_detsim::hover::{
    self, ClimbTuning, GRAVITY, HoverParams, HoverState, apply_hull_hop, compute_lean, hop_launch_speed,
    try_step_tuned,
};
use regia_detsim::tick::tick_dt;
use serde::{Deserialize, Serialize};

use super::formats::{pretty, text};
use super::{FuzzInput, FuzzTarget};

/// Most ticks one input may ask for.
const MAX_TICKS: usize = 64;
/// Largest `dt` of the finite-state domain (a 30 Hz tick).
const DOMAIN_DT_MAX: f32 = 1.0 / 30.0;
/// Largest floor height of the finite-state domain, metres.
const DOMAIN_FLOOR_MAX: f32 = 1.0e9;

/// One tick of the run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Tick {
    /// `[throttle, strafe, steer]`.
    input: [f32; 3],
    /// Floor height under the hull.
    floor: f32,
    /// Step length, seconds.
    dt: f32,
    /// Press the hop before this tick.
    #[serde(default)]
    hop: bool,
}

/// A hull and the ticks it flies.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HullRun {
    params: HoverParams,
    /// Fly the clamped flight rules instead of air time.
    #[serde(default)]
    clamped: bool,
    ticks: Vec<Tick>,
}

fn fixture_run() -> HullRun {
    let dt = tick_dt();
    let mut ticks = Vec::new();
    for i in 0..12_u8 {
        let f = f32::from(i);
        ticks.push(Tick {
            input: [1.0, if i % 3 == 0 { -0.5 } else { 0.25 }, 0.1 * f - 0.5],
            floor: if i == 7 { f32::NAN } else { 0.25 * f },
            dt: if i == 9 { 0.0 } else { dt },
            hop: i == 4,
        });
    }
    HullRun {
        params: HoverParams::DEFAULT_HULL,
        clamped: false,
        ticks,
    }
}

fn state_finite(s: &HoverState) -> bool {
    [
        s.velocity[0],
        s.velocity[1],
        s.velocity[2],
        s.vertical_vel,
        s.yaw,
        s.yaw_rate,
        s.altitude,
        s.prev_terrain.unwrap_or(0.0),
        s.hop_cooldown,
        s.ground_climb,
        s.slope,
        s.roll,
        s.roll_rate,
        s.pitch,
        s.coast_drop_left,
        s.ground_pitch,
        s.ground_roll,
        s.cushion_pitch,
        s.cushion_pitch_rate,
    ]
    .iter()
    .all(|w| w.is_finite())
}

/// Fly `ticks`. In the domain run (`domain`), a `dt` outside `(0, 1/30]` is the 60 Hz tick and a
/// floor beyond ±1e9 m is clamped (a NaN floor stays: the step's guard reads it), and a non-finite
/// state word or a recovery is a bug.
fn fly(params: &HoverParams, climb: &ClimbTuning, ticks: &[Tick], domain: bool) -> Result<(), String> {
    let launch = hop_launch_speed(GRAVITY, 0.0, tick_dt());
    let mut s = HoverState::default();
    for (i, t) in ticks.iter().enumerate() {
        let (dt, floor) = if domain {
            let dt = if t.dt > 0.0 && t.dt <= DOMAIN_DT_MAX {
                t.dt
            } else {
                tick_dt()
            };
            let floor = if t.floor.abs() > DOMAIN_FLOOR_MAX {
                DOMAIN_FLOOR_MAX.copysign(t.floor)
            } else {
                t.floor
            };
            (dt, floor)
        } else {
            (t.dt, t.floor)
        };
        if t.hop {
            apply_hull_hop(&mut s, launch);
        }
        let report = try_step_tuned(params, &mut s, &t.input, floor, dt, GRAVITY, 0.0, climb);
        let _ = compute_lean(params, &s, &t.input);
        let _ = hover::hull_in_air(params, &s);
        if domain && (report.recovered || !state_finite(&s)) {
            return Err(format!(
                "{}a sanitized hull left the finite state at tick {i} (recovered {}): {s:?}",
                super::BUG_PREFIX,
                report.recovered
            ));
        }
    }
    Ok(())
}

fn load(s: &str) -> Result<String, String> {
    let run: HullRun = ron::from_str(s).map_err(|e| e.to_string())?;
    if run.ticks.len() > MAX_TICKS {
        return Err(format!("{} ticks (at most {MAX_TICKS})", run.ticks.len()));
    }
    let climb = if run.clamped {
        ClimbTuning::CLAMPED_1014
    } else {
        ClimbTuning::AIR_TIME
    };
    // The hull as written: any numbers, any ticks; it must only terminate without a panic.
    let _ = run.params.validate();
    fly(&run.params, &climb, &run.ticks, false)?;
    // Sanitized: valid, and finite throughout the documented domain.
    let sane = run.params.sanitized();
    sane.validate()
        .map_err(|e| format!("{}sanitized hull fails validate: {e}", super::BUG_PREFIX))?;
    fly(&sane, &climb, &run.ticks, true)?;
    pretty(&run)
}

/// The frozen v1 fixture (`format-corpus/hoverhull/v1.ron`).
fn hoverhull_fixture() -> Vec<u8> {
    include_bytes!("../../tests/fixtures/format-corpus/hoverhull/v1.ron").to_vec()
}

/// Bytes to a hull: parse, validate, and write the canonical text back. A bad input is an error.
/// A hull that loaded and then fails to write or reload is an engine bug.
fn hoverhull_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "parse_error: hover hull text is not UTF-8".to_string())?;
    let asset = regia_framework::det_hover::HoverHullAsset::from_ron(text).map_err(|err| err.to_string())?;
    asset
        .validate()
        .map_err(|err| format!("{}{err}", super::BUG_PREFIX))?;
    let canonical = asset
        .to_ron()
        .map_err(|err| format!("{}{err}", super::BUG_PREFIX))?;
    let again = regia_framework::det_hover::HoverHullAsset::from_ron(&canonical)
        .map_err(|err| format!("{}canonical hover hull does not reload: {err}", super::BUG_PREFIX))?;
    if again != asset {
        return Err(format!(
            "{}canonical hover hull reloaded as a different asset",
            super::BUG_PREFIX
        ));
    }
    Ok(canonical.into_bytes())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "hover_params_step",
            None,
            0,
            true,
            || pretty(&fixture_run()).unwrap_or_default(),
            load,
        ),
        FuzzTarget {
            name: "hoverhull_ron",
            format: Some("hoverhull"),
            format_version: regia_framework::det_hover::HOVERHULL_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: hoverhull_fixture,
                load: hoverhull_load,
            },
        },
    ]
}
