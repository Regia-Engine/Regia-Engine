// SPDX-License-Identifier: MPL-2.0
//! Locomotion follow-ups of the v0.7 plan item L-05 (ADR-10130), through the dispatcher:
//! the swing bound holds on seeded rubble with steering, and `params.posture` sets the body
//! posture of an `anim.gait_simulate` run (`book/agents/locomotion-quality.md`).

use regia_agent::{LocalReply, call_local};
use regia_animation::AnimationPlugin;
use regia_app::prelude::{App, MinimalPlugins};
use serde_json::{Value, json};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app
}

fn reply(app: &mut App, method: &str, params: Value) -> Result<Value, String> {
    match call_local(app.world_mut(), method, params, Some("locomotion-followups")) {
        LocalReply::Done(Ok(value)) => Ok(value),
        LocalReply::Done(Err(error)) => {
            let data = error.data.as_ref().expect("error data");
            assert!(data.get("hint").and_then(Value::as_str).is_some(), "{error:?}");
            Err(data["code"].as_str().expect("code").to_string())
        }
        reply => panic!("{method}: {reply:?}"),
    }
}

fn f(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

/// The largest one-step foot move in a `trace_every: 1` trace, with its step, foot and the
/// planted flags before and after.
fn worst_jump(trace: &[Value]) -> (f64, u64, usize, String) {
    let mut worst = (0.0, 0, 0, String::new());
    for pair in trace.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let feet = |row: &Value| -> Vec<[f64; 3]> {
            row["feet"]
                .as_array()
                .expect("feet")
                .iter()
                .map(|p| [f(&p[0]), f(&p[1]), f(&p[2])])
                .collect()
        };
        for (i, (p, q)) in feet(a).iter().zip(feet(b)).enumerate() {
            let d = ((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2) + (q[2] - p[2]).powi(2)).sqrt();
            if d > worst.0 {
                worst = (
                    d,
                    b["step"].as_u64().unwrap_or(0),
                    i,
                    format!(
                        "planted {} -> {}, {p:?} -> {q:?}",
                        a["planted"][i], b["planted"][i]
                    ),
                );
            }
        }
    }
    worst
}

/// Item 1 of L-05 (`sprint-integ-anim-v07a` report): hexapod on rubble (amplitude 0.08,
/// cell 0.4, seed 11), walking and steering for 300 steps and then stopping, 480 steps. Every
/// foot move in one step stays within the published `swing_bound`.
#[test]
fn rubble_seed_11_steering_stays_within_the_swing_bound() {
    let mut app = app();
    let run = reply(
        &mut app,
        "anim.gait_simulate",
        json!({
            "steps": 480,
            "trace_every": 1,
            "ground": { "kind": "rubble", "amplitude": 0.08, "cell": 0.4, "seed": 11 },
            "inputs": [[1.0, 0.0, 0.2, 300], [0.0, 0.0, 0.0]]
        }),
    )
    .unwrap_or_else(|code| panic!("refused as {code}"));
    let metrics = &run["metrics"];
    let (jump, bound) = (f(&metrics["max_foot_jump"]), f(&metrics["swing_bound"]));
    let trace = run["trace"].as_array().expect("trace");
    let (worst, step, foot, how) = worst_jump(trace);
    assert!(
        jump <= bound + 1e-4,
        "max_foot_jump {jump:.4} > swing_bound {bound:.4}; worst {worst:.4} at step {step} foot {foot}: {how}"
    );
}

/// The same check over more seeds, steer signs and rubble sizes, so a fix holds beyond the
/// one reported seed.
#[test]
fn rubble_sweep_stays_within_the_swing_bound() {
    let mut app = app();
    let mut failures = Vec::new();
    for seed in [1_u64, 5, 11, 23, 42] {
        for steer in [-0.4_f64, 0.2, 0.6] {
            for amplitude in [0.04_f64, 0.08, 0.15] {
                let run = reply(
                    &mut app,
                    "anim.gait_simulate",
                    json!({
                        "steps": 480,
                        "ground": { "kind": "rubble", "amplitude": amplitude, "cell": 0.4, "seed": seed },
                        "inputs": [[1.0, 0.0, steer, 300], [0.0, 0.0, 0.0]]
                    }),
                )
                .unwrap_or_else(|code| panic!("seed {seed} refused as {code}"));
                let m = &run["metrics"];
                let (jump, bound) = (f(&m["max_foot_jump"]), f(&m["swing_bound"]));
                if jump > bound + 1e-4 {
                    failures.push(format!(
                        "seed {seed} steer {steer} amplitude {amplitude}: {jump:.4} > {bound:.4}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Item 2 of L-05: `params.posture` sets the body posture (the `.gait.ron` posture fields), so
/// a tilting walker needs no file. On a slope the body pitches; without it, it stays upright.
#[test]
fn params_posture_tilts_the_body_on_a_slope() {
    let mut app = app();
    let slope = json!({ "kind": "slope", "rise_z": 0.3 });
    let upright = reply(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 240, "ground": slope }),
    )
    .unwrap_or_else(|code| panic!("upright refused as {code}"));
    assert_eq!(
        f(&upright["final"]["attitude"]["pitch"]),
        0.0,
        "{}",
        upright["final"]
    );
    let tilted = reply(
        &mut app,
        "anim.gait_simulate",
        json!({
            "steps": 240,
            "ground": slope,
            "params": { "posture": {
                "swing_weight": 1.0, "height_tau": 0.1, "attitude_weight": 1.0,
                "max_pitch": 0.6, "max_roll": 0.6, "attitude_tau": 0.1
            } }
        }),
    )
    .unwrap_or_else(|code| panic!("posture refused as {code}"));
    let pitch = f(&tilted["final"]["attitude"]["pitch"]);
    assert!(
        pitch.abs() > 0.05,
        "a posture walker on a 0.3 grade pitches: {}",
        tilted["final"]
    );
}

/// Unset posture fields keep the upright values; `height_slew: null` is no limit; an unknown
/// posture field or an out-of-range value is `gait_settings`.
#[test]
fn params_posture_fields_are_checked() {
    let mut app = app();
    let partial = reply(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 10, "params": { "posture": { "height_tau": 0.2, "height_slew": null } } }),
    );
    assert!(partial.is_ok(), "{partial:?}");
    for posture in [
        json!({ "tilt": 1.0 }),
        json!({ "attitude_weight": 2.0 }),
        json!({ "max_pitch": 2.0 }),
        json!({ "height_tau": "slow" }),
        json!("upright"),
    ] {
        let refused = reply(
            &mut app,
            "anim.gait_simulate",
            json!({ "steps": 10, "params": { "posture": posture } }),
        );
        assert_eq!(refused, Err("gait_settings".to_string()), "posture {posture}");
    }
}
