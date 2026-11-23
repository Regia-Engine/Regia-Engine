// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/handling_audit.rs, commit c9513623f), ADR-8012.
// Mark: as-is. Named changes: the caller passes HoverParams (A-8 names) instead of a class table;
// reference-hull names, the on-foot row and the jetpack row are not part of the engine rig. The
// `REGIA_HANDLING_AUDIT_OUT` dump is optional.

//! Feel-target measurement rig and the handling audit (plan row S-08, ADR-8393).
//!
//! `rig_reports_the_feel_targets` is the acceptance: the rig reports the engine default hull's
//! feel targets. `handling_audit_table` is the handling audit ported from Direct Order 27
//! `ir-sim/tests/handling_audit.rs` (regia-absorb-4, commit c9513623f): the same tier
//! table (top speed, launch, coast, brake, yaw, strafe, hover height, crest), with the caller
//! passing hull parameters instead of a game class table. Reference-hull names and the on-foot and
//! jetpack rows are not part of the engine rig.

use regia_detsim::handling::{self, DT, Report, flat, fly, hover_state_at, measure_with};
use regia_detsim::hover::{HoverLimits, HoverParams};
use regia_detsim::math;
use regia_detsim::site_physics::EARTH_G;

/// The engine's default hop apex (`HoverLimits::DEFAULT`, the value of the deprecated `hover::HOP_APEX_M`, ADR-8349).
const HOP_APEX_M: f32 = HoverLimits::DEFAULT.hop_apex_m;

/// One tier of one hull, flat ground, Earth gravity. The columns match the source audit.
#[derive(Debug, Clone, Copy)]
struct Tier {
    top_m_s: f32,
    t90_s: f32,
    coast_s: f32,
    coast_m: f32,
    brake_s: f32,
    brake_m: f32,
    spin_rad_s: f32,
    turn_rad_s: f32,
    strafe_m_s: f32,
    strafe_t90_s: f32,
    strafe_stop_m: f32,
    hover_m: f32,
    crest_peak_m: f32,
}

/// Strafe stop distance: 5 s of strafe, then release, until lateral speed is under 0.3 m/s.
fn strafe_stop(p: &HoverParams) -> f32 {
    let mut st = hover_state_at(p, 0.0);
    fly(p, &mut st, EARTH_G, 5.0, &|_| [0.0, 1.0, 0.0], &flat);
    let stop = fly(p, &mut st, EARTH_G, 8.0, &|_| [0.0, 0.0, 0.0], &flat);
    let at = stop
        .iter()
        .find(|s| s.lat.abs() < 0.3)
        .map(|s| s.dist)
        .unwrap_or(f32::NAN);
    let from = stop.first().map(|s| s.dist).unwrap_or(0.0);
    at - from
}

fn measure_tier(p: &HoverParams) -> Tier {
    let m = measure_with(p, EARTH_G, false);
    let mut st = hover_state_at(p, 0.0);
    let rest = fly(p, &mut st, EARTH_G, 4.0, &|_| [0.0; 3], &flat);
    let ticks_in_one_second = (1.0 / DT) as usize;
    let samples = rest.len().max(1);
    let window = ticks_in_one_second.min(samples);
    let start = rest.len().saturating_sub(window);
    let mut sum = 0.0_f32;
    for sample in &rest[start..] {
        sum += sample.altitude;
    }
    let hover_m = sum / window as f32;
    Tier {
        top_m_s: m.cruise_m_s,
        t90_s: m.t_to_cruise_s,
        coast_s: m.coast_time_s,
        coast_m: m.coast_dist_m,
        brake_s: m.brake_time_s,
        brake_m: m.brake_dist_m,
        spin_rad_s: m.spin_rate_rad_s,
        turn_rad_s: m.turn_rate_cruise_rad_s,
        strafe_m_s: m.strafe_speed_m_s,
        strafe_t90_s: m.strafe_t90_s,
        strafe_stop_m: strafe_stop(p),
        hover_m,
        crest_peak_m: m.crest_peak_m,
    }
}

fn tier_line(name: &str, t: &Tier) -> String {
    format!(
        "{name} | {:.2} | {:.2} | {:.2} | {:.1} | {:.2} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} | {:.2} | {:.2}",
        t.top_m_s,
        t.t90_s,
        t.coast_s,
        t.coast_m,
        t.brake_s,
        t.brake_m,
        t.spin_rad_s,
        t.turn_rad_s,
        t.strafe_m_s,
        t.strafe_t90_s,
        t.strafe_stop_m,
        t.hover_m,
        t.crest_peak_m,
    )
}

fn finite_positive(label: &str, v: f32) {
    assert!(v.is_finite() && v > 0.0, "{label}: rig failed ({v})");
}

/// The rig's feel-target report for the engine default hull: every column is finite, the cruise
/// target is the hull's own forward cap, the hop is the engine's 3 m pulse, and a second world
/// with the thrust floor reports the same flat-ground cruise.
#[test]
fn rig_reports_the_feel_targets() {
    let params = HoverParams::DEFAULT_HULL;
    let report = handling::measure(&params, EARTH_G);
    assert_feel(&params, &report, "earth");

    // The value of the deprecated `site_physics::TITAN_G` (ADR-8349: a game keeps its bodies' gravity).
    let titan_g = 1.35_f32;
    assert!(
        handling::WORLDS
            .iter()
            .any(|(name, g)| { *name == "titan" && g.to_bits() == titan_g.to_bits() })
    );
    assert!(
        handling::WORLDS
            .iter()
            .any(|(name, g)| *name == "earth" && g.to_bits() == EARTH_G.to_bits())
    );
    let titan = handling::measure(&params, titan_g);
    assert_eq!(titan.cruise_m_s.to_bits(), report.cruise_m_s.to_bits());
    assert!(
        (titan.t_to_cruise_s - report.t_to_cruise_s).abs() < 0.05,
        "titan cruise time {} vs earth {}",
        titan.t_to_cruise_s,
        report.t_to_cruise_s
    );
    assert!(
        (titan.brake_dist_m - report.brake_dist_m).abs() < 0.05,
        "titan brake {} vs earth {}",
        titan.brake_dist_m,
        report.brake_dist_m
    );
    assert!(
        (titan.hop_apex_m - HOP_APEX_M).abs() < 0.15,
        "titan hop {}",
        titan.hop_apex_m
    );

    let again = handling::measure(&params, EARTH_G);
    assert_eq!(report.t_to_cruise_s.to_bits(), again.t_to_cruise_s.to_bits());
    assert_eq!(report.hop_apex_m.to_bits(), again.hop_apex_m.to_bits());
    assert_eq!(report.coast_dist_m.to_bits(), again.coast_dist_m.to_bits());

    let dumped = handling::format_report(&report);
    for column in [
        "cruise_m_s",
        "t_to_cruise_s",
        "coast_dist_m",
        "brake_time_s",
        "hop_apex_m",
        "strafe_speed_m_s",
    ] {
        assert!(dumped.contains(column), "dump missing {column}: {dumped}");
    }
    println!("regia-handling-rig {dumped}");
}

fn assert_feel(params: &HoverParams, report: &Report, world: &str) {
    assert_eq!(
        report.cruise_m_s.to_bits(),
        params.max_forward_speed.to_bits(),
        "{world}: cruise target {} vs {}",
        report.cruise_m_s,
        params.max_forward_speed
    );
    finite_positive(world, report.t_to_cruise_s);
    assert!(
        report.t_to_cruise_s < 6.0,
        "{world}: 0-90% took {} s",
        report.t_to_cruise_s
    );
    finite_positive(world, report.boost_m_s);
    finite_positive(&format!("{world} coast s"), report.coast_time_s);
    finite_positive(&format!("{world} coast m"), report.coast_dist_m);
    finite_positive(&format!("{world} brake s"), report.brake_time_s);
    finite_positive(&format!("{world} brake m"), report.brake_dist_m);
    assert!(
        report.brake_time_s < report.coast_time_s,
        "{world}: brake {} s was not quicker than coast {} s",
        report.brake_time_s,
        report.coast_time_s
    );
    let spin_gap = (report.spin_rate_rad_s - params.yaw_rate_at_rest).abs();
    assert!(
        spin_gap <= 0.05 * params.yaw_rate_at_rest,
        "{world}: spin {} vs cap {}",
        report.spin_rate_rad_s,
        params.yaw_rate_at_rest
    );
    let strafe_gap = (report.strafe_speed_m_s - params.max_strafe_speed).abs();
    assert!(
        strafe_gap <= 0.05 * params.max_strafe_speed,
        "{world}: strafe {} vs cap {}",
        report.strafe_speed_m_s,
        params.max_strafe_speed
    );
    assert!(
        report.turn_rate_cruise_rad_s.is_finite() && report.turn_rate_cruise_rad_s > 0.0,
        "{world}"
    );
    assert!(report.lean_pitch_deg.is_finite(), "{world} pitch");
    assert!(report.lean_roll_turn_deg.is_finite(), "{world} turn roll");
    assert!(report.lean_roll_strafe_deg.is_finite(), "{world} strafe roll");
    assert!(
        report.slope_speed_keep.is_finite() && report.slope_speed_keep > 0.0,
        "{world} slope"
    );
    assert!(
        report.crest_peak_m.is_finite() && report.crest_peak_m > 0.0,
        "{world} crest"
    );
    assert!(report.uphill_coast_ratio.is_finite(), "{world} uphill");
    assert!(
        (report.hop_apex_m - HOP_APEX_M).abs() < 0.15,
        "{world}: hop {} m, engine apex {HOP_APEX_M}",
        report.hop_apex_m
    );
}

/// The handling audit: one driven hull and one plain-throttle hull, both caller-supplied. It
/// prints the tier table and asserts only that the rig ran and the hop is the engine's 3 m pulse.
/// `REGIA_HANDLING_AUDIT_OUT` writes that table (atomic rename) when set.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "REGIA_HANDLING_AUDIT_OUT only names where the table is written for review; the checks are the same (ADR-8367)"
)]
fn handling_audit_table() {
    let driven_params = HoverParams::DEFAULT_HULL;
    let mut plain_params = driven_params;
    plain_params.thrust_taper = 0.0;
    plain_params.turbo_scale = 1.0;
    let driven = measure_tier(&driven_params);
    let plain = measure_tier(&plain_params);
    let hop = measure_with(&driven_params, EARTH_G, false).hop_apex_m;

    let mut out = String::from(
        "hull/tier | top m/s | t90 s | coast s | coast m | brake s | brake m | spin rad/s | turn rad/s | strafe m/s | strafe t90 s | strafe stop m | hover m | crest peak m\n",
    );
    out.push_str(&tier_line("sample driven", &driven));
    out.push('\n');
    out.push_str(&tier_line("sample plain", &plain));
    out.push('\n');
    out.push_str(&format!("sample hop apex m: {hop:.2}\n"));
    println!("\n{out}");

    if let Ok(path) = std::env::var("REGIA_HANDLING_AUDIT_OUT") {
        let wrote = handling::write_report_atomic(std::path::Path::new(&path), &out);
        assert!(wrote.is_ok(), "audit write: {wrote:?}");
    }

    finite_positive("sample driven", driven.top_m_s);
    finite_positive("sample plain", plain.top_m_s);
    assert!(
        (hop - HOP_APEX_M).abs() < 0.2,
        "hull hop is the engine's {HOP_APEX_M} m, got {hop}"
    );
}

/// The same scripted trace, twice, hashes to the same bits.
#[test]
fn a_scripted_trace_is_deterministic() {
    let run = || {
        let p = HoverParams::DEFAULT_HULL;
        let mut st = hover_state_at(&p, 0.0);
        let trace = fly(&p, &mut st, 1.35, 5.0, &|t| [1.0, 0.3, math::sin(t * 2.0)], &flat);
        trace
            .iter()
            .map(|s| s.dist.to_bits())
            .fold(0u32, |a, b| a.rotate_left(5) ^ b)
    };
    assert_eq!(run(), run());
}
