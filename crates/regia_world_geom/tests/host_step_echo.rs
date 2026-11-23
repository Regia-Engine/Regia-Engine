// SPDX-License-Identifier: MPL-2.0
//! K4 guard (plan row W-29; DO27's ruling on the Stage B known-issue plans, 2026-11-17: K4 is not
//! changed in Stage B). The host step reads the floor twice, in this order: the hover step
//! (`regia_detsim::hover`, S-07) reads it at the pre-move XZ for the spring and the climb lead, and
//! the ground pass (`compat::hover_ground::ground_hover_hull`'s arithmetic) reads it at the
//! post-move XZ. A floor step of `h` under a moving hull therefore shows as `+h` on the tick the
//! hull crosses it and as an echo of about `-h` on the next tick, after which the spring climbs
//! back. This test pins that order and that the echo lasts exactly one tick, as a documented,
//! measured property. Collapsing the two reads into one changes handling on every slope; it needs
//! a DO27 handling lane with a parity run and its own sim-version bump, not this suite.

use regia_detsim::float::det_max;
use regia_detsim::hover::{self, HoverParams, HoverState};
use regia_detsim::tick::TickRate;
use regia_world_geom::compat::hover_ground::LEDGE_M;

/// The floor under the drive: flat at 0, a step up of `h` at x = 10 m.
fn floor(x: f32, h: f32) -> f32 {
    if x < 10.0 { 0.0 } else { h }
}

/// One host tick in DO27's order. Returns the displayed body height.
fn host_tick(
    params: &HoverParams,
    s: &mut HoverState,
    x: &mut f32,
    body: &mut f32,
    h: f32,
    speed: f32,
    dt: f32,
) -> f32 {
    // 1. The hover step at the pre-move XZ.
    let pre = floor(*x, h);
    hover::step(params, s, &[0.0, 0.0, 0.0], pre, dt);
    // The mover leaves the body at the pre-move floor plus the new altitude (the host's hint).
    let hint_y = pre + s.altitude;
    // 2. The move.
    *x += speed * dt;
    // 3. The ground pass at the post-move XZ: the ledge rule, else ride at the altitude.
    let post = floor(*x, h);
    let prev_floor = hint_y - s.altitude;
    let y = if (post - prev_floor).abs() > LEDGE_M {
        let lifted = det_max(hint_y - post, 0.0);
        s.altitude = lifted;
        s.prev_terrain = Some(post);
        if lifted == 0.0 {
            s.vertical_vel = det_max(s.vertical_vel, 0.0);
        }
        post + lifted
    } else {
        post + s.altitude
    };
    *body = y;
    y
}

/// The displayed height, tick by tick, of a hull driving over a step of `h` at `speed`.
fn drive(h: f32, speed: f32) -> Vec<f32> {
    let params = HoverParams::DEFAULT_HULL;
    let dt = TickRate::DEFAULT.dt();
    let mut s = HoverState {
        altitude: params.ride_height,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    };
    let mut x = 9.0f32;
    let mut body = params.ride_height;
    // Settle on the flat first, standing still, so the spring is at rest.
    for _ in 0..120 {
        let _ = host_tick(&params, &mut s, &mut x, &mut body, h, 0.0, dt);
    }
    let mut ys = vec![body];
    for _ in 0..600 {
        ys.push(host_tick(&params, &mut s, &mut x, &mut body, h, speed, dt));
    }
    ys
}

#[test]
fn floor_step_echo_is_one_tick() {
    for h in [0.1f32, 0.25, 0.5] {
        let ys = drive(h, 6.0);
        let d: Vec<f32> = ys.windows(2).map(|w| w[1] - w[0]).collect();
        // The crossing tick: the ground pass reads the new floor first, so the body shows +h.
        let cross = d.iter().position(|v| *v > h * 0.5).expect("the step shows");
        println!(
            "K4 h={h}: crossing tick {cross}, deltas {:?}",
            &d[cross.saturating_sub(1)..(cross + 6).min(d.len())]
        );
        assert!(
            (d[cross] - h).abs() <= h * 0.05,
            "the crossing tick shows the step: {} for {h}",
            d[cross]
        );
        // The echo: the next hover step reads the step at its pre-move XZ and gives most of it back.
        assert!(d[cross + 1] < -h * 0.5, "the echo tick: {} for {h}", d[cross + 1]);
        // Exactly one tick: the tick after the echo the spring climbs, and no later tick steps down
        // by more than a tenth of the step (the spring's own settle after its overshoot is under a
        // millimetre a tick; measured 2026-11-18: h = 0.1 gives +0.100, -0.0965, then +0.006 rising).
        assert!(
            d[cross + 2] > 0.0,
            "the tick after the echo climbs: {} for {h}",
            d[cross + 2]
        );
        for (k, v) in d.iter().enumerate().skip(cross + 2) {
            assert!(*v >= -0.1 * h, "a second echo at tick {k}: {v} for {h}");
        }
        // Before the crossing nothing moves (flat floor at rest).
        for v in &d[..cross] {
            assert!(v.abs() <= 1e-5, "the flat drive moved {v} before the step");
        }
        // The hull ends at its ride over the new floor (ten seconds later).
        let end = ys[ys.len() - 1];
        assert!(
            (end - (h + HoverParams::DEFAULT_HULL.ride_height)).abs() <= 0.05,
            "end {end} for {h}"
        );
    }
}
