// SPDX-License-Identifier: MPL-2.0
use std::collections::HashSet;
use std::time::Duration;

use regia_app::{MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_core::math::wake::{WakeBody, WakeShape};
use regia_physics::{ExternalImpulse, RigidBody};
use regia_scene::{GlobalTransform, Transform};

use super::spray::{SpraySpec, SpraySurface, plan_spray, spray_effect};
use super::*;
use crate::shots::{CameraPath, LookAt, PathInterp, PathKey, Shot, ShotCamera, ShotPlugin, Track};

/// One shot flying along +X at `speed` m/s, 10 m up, for `duration` seconds.
fn flight(speed: f32, duration: f32) -> ShotSequence {
    ShotSequence {
        shots: vec![Shot {
            name: "pass".into(),
            duration,
            camera: ShotCamera {
                path: CameraPath {
                    interp: PathInterp::Linear,
                    keys: vec![
                        PathKey {
                            time: 0.0,
                            position: [0.0, 10.0, 0.0],
                        },
                        PathKey {
                            time: duration,
                            position: [speed * duration, 10.0, 0.0],
                        },
                    ],
                    look_at: LookAt::Heading,
                },
                fov: Track::default(),
                roll: Track::default(),
                dof: None,
                rotation: Vec::new(),
            },
            fade_in: 0.0,
            fade_out: 0.0,
            fade_color: [0.0; 3],
        }],
        ..ShotSequence::default()
    }
}

#[test]
fn spec_json_takes_defaults_and_refuses_unknown_fields_and_newer_versions() {
    let spec = CameraBodySpec::from_json(r#"{ "wake": { "radius_m": 2.0 } }"#).expect("spec");
    assert_eq!(spec.wake.radius_m, 2.0);
    assert_eq!(spec.wake.range_m, WakeShape::default().range_m);
    assert!(spec.enabled && spec.push.enabled && spec.spray.enabled);
    assert!(CameraBodySpec::from_json(r#"{ "wake": { "radius": 2.0 } }"#).is_err());
    assert!(matches!(
        CameraBodySpec::from_json(r#"{ "version": 99 }"#),
        Err(CameraBodySpecError::Version { found: 99 })
    ));
    // Out-of-range values are clamped, not refused.
    let clamped =
        CameraBodySpec::from_json(r#"{ "wake": { "gain": 9.0 }, "velocity_step_s": -1.0 }"#).expect("spec");
    assert_eq!(clamped.wake.gain, 1.0);
    assert!(clamped.velocity_step_s > 0.0);
}

#[test]
fn path_velocity_is_a_central_difference_and_one_sided_at_cuts() {
    let path = FnPath {
        position: |t: f64| Vec3::new(30.0 * t as f32, 5.0, -4.0 * t as f32),
        span: (0.0, 10.0),
    };
    let b = path.body(5.0, DEFAULT_VELOCITY_STEP_S).expect("body");
    assert!(
        (b.velocity - Vec3::new(30.0, 0.0, -4.0)).length() < 1.0e-2,
        "{}",
        b.velocity
    );
    let end = path.body(10.0, DEFAULT_VELOCITY_STEP_S).expect("body");
    assert!((end.velocity.x - 30.0).abs() < 1.0e-2, "one-sided at the end");

    // Two shots: a cut at t = 2 from x = 200 back to x = 0. The velocity on either side of the
    // cut is the shot's own, never the jump.
    let mut seq = flight(100.0, 2.0);
    let second = flight(50.0, 2.0).shots.remove(0);
    seq.shots.push(Shot {
        name: "second".into(),
        ..second
    });
    let before = seq.body(2.0 - 1.0e-3, DEFAULT_VELOCITY_STEP_S).expect("body");
    let after = seq.body(2.0, DEFAULT_VELOCITY_STEP_S).expect("body");
    assert!((before.velocity.x - 100.0).abs() < 0.5, "{}", before.velocity);
    assert!((after.velocity.x - 50.0).abs() < 0.5, "{}", after.velocity);
}

fn body_app(sequence: ShotSequence) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ShotPlugin, CameraBodyPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    {
        let mut player = app.world_mut().resource_mut::<ShotPlayer>();
        player.load(sequence);
        player.play();
    }
    app
}

#[test]
fn the_body_follows_the_shot_player_deterministically() {
    let run = || {
        let mut app = body_app(flight(120.0, 5.0));
        let mut reports = Vec::new();
        for _ in 0..30 {
            app.update();
            reports.push(app.world().resource::<CameraBody>().report().clone());
        }
        reports
    };
    let a = run();
    let b = run();
    assert_eq!(a, b, "two runs of the same frames give the same body");
    let last = a.last().expect("frames");
    assert_eq!(last.source, CameraBodySource::Sequence);
    assert!((last.speed_m_s - 120.0).abs() < 0.5, "{}", last.speed_m_s);
    assert!(last.body_gust_m_s > 0.0);
    assert!(last.frames >= 29);
}

#[test]
fn host_drive_overrides_and_release_hands_back() {
    let mut body = CameraBody::new(CameraBodySpec::default());
    let state = WakeBody {
        position: Vec3::new(0.0, 5.0, 0.0),
        velocity: Vec3::new(60.0, 0.0, 0.0),
    };
    body.drive(1.0, state);
    assert_eq!(body.report().source, CameraBodySource::Host);
    assert_eq!(body.state(), Some(state));
    assert!(body.wake_words().is_some());
    // Just passed: a point 2 m behind and 2 m to the side gets a gust; one ahead does not.
    assert!(body.wake_at(Vec3::new(-2.0, 5.0, 2.0)).gust.length() > 0.0);
    assert_eq!(body.wake_at(Vec3::new(5.0, 5.0, 2.0)).gust, Vec3::ZERO);
    body.release();
    let off = CameraBody::new(CameraBodySpec {
        enabled: false,
        ..CameraBodySpec::default()
    });
    assert!(off.state().is_none() && off.wake_words().is_none());
}

#[test]
fn loose_bodies_in_the_wake_get_a_drag_impulse() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )))
        .insert_resource(CameraBody::new(CameraBodySpec::default()))
        .add_systems(Update, push::push_rigid_bodies);
    app.world_mut().resource_mut::<CameraBody>().drive(
        0.0,
        WakeBody {
            position: Vec3::new(0.0, 3.0, 0.0),
            velocity: Vec3::new(80.0, 0.0, 0.0),
        },
    );
    let at = |x: f32, z: f32| {
        let t = Transform {
            translation: Vec3::new(x, 0.5, z),
            ..Transform::default()
        };
        (t, GlobalTransform(t.compute_affine()))
    };
    let near = app.world_mut().spawn((RigidBody::dynamic(), at(-1.0, 2.0))).id();
    let ahead = app.world_mut().spawn((RigidBody::dynamic(), at(10.0, 2.0))).id();
    let fixed = app
        .world_mut()
        .spawn((RigidBody::kinematic(), at(-1.0, 2.0)))
        .id();
    app.update();
    let j = app.world().get::<ExternalImpulse>(near).expect("pushed").impulse;
    assert!(j.x > 0.0, "dragged along the travel: {j}");
    assert!(j.z > 0.0, "and pushed aside: {j}");
    assert!(
        app.world().get::<ExternalImpulse>(ahead).is_none(),
        "ahead of the front"
    );
    assert!(
        app.world().get::<ExternalImpulse>(fixed).is_none(),
        "kinematic bodies stay"
    );
    let report = app.world().resource::<CameraBody>().report();
    assert_eq!(report.pushed_bodies, 1);
    assert!(report.pushed_impulse > 0.0);
}

fn flat(_x: f32, _z: f32) -> Option<f32> {
    Some(0.0)
}

#[test]
fn spray_lifts_each_cell_once_per_pass_and_only_low_and_fast() {
    let shape = WakeShape::default();
    let spec = SpraySpec::default();
    let dt = 1.0 / 60.0;
    let at = |frame: u32, height: f32, speed: f32| WakeBody {
        position: Vec3::new(speed * frame as f32 * dt, height, 0.0),
        velocity: Vec3::new(speed, 0.0, 0.0),
    };
    let mut cells = HashSet::new();
    let mut total = 0;
    for frame in 0..90u32 {
        let puffs = plan_spray(
            &spec,
            &shape,
            &at(frame, 6.0, 250.0),
            dt,
            u64::from(frame),
            &flat,
            None,
        );
        assert!(puffs.len() <= spec.max_puffs_per_frame as usize);
        for p in &puffs {
            assert!(cells.insert(p.cell), "cell {:?} lifted twice", p.cell);
            assert!(p.gust_m_s >= spec.lift_m_s);
            assert_eq!(p.surface, SpraySurface::Dust);
        }
        total += puffs.len();
    }
    assert!(total > 50, "a fast low pass leaves a trail: {total}");
    // Too high, too slow, or a still frame: nothing.
    assert!(plan_spray(&spec, &shape, &at(10, 300.0, 250.0), dt, 10, &flat, None).is_empty());
    assert!(plan_spray(&spec, &shape, &at(10, 6.0, 5.0), dt, 10, &flat, None).is_empty());
    assert!(plan_spray(&spec, &shape, &at(10, 6.0, 250.0), 0.0, 10, &flat, None).is_empty());
    // Pure: the same frame plans the same puffs.
    let a = plan_spray(&spec, &shape, &at(40, 6.0, 250.0), dt, 40, &flat, None);
    let b = plan_spray(&spec, &shape, &at(40, 6.0, 250.0), dt, 40, &flat, None);
    assert_eq!(a, b);
    // A surface function picks the effect; `None` surfaces lift nothing.
    let snow = |_x: f32, _z: f32| SpraySurface::Snow;
    let s = plan_spray(&spec, &shape, &at(40, 6.0, 250.0), dt, 40, &flat, Some(&snow));
    assert!(s.iter().all(|p| p.surface == SpraySurface::Snow) && !s.is_empty());
    let rock = |_x: f32, _z: f32| SpraySurface::None;
    assert!(plan_spray(&spec, &shape, &at(40, 6.0, 250.0), dt, 40, &flat, Some(&rock)).is_empty());
}

#[test]
fn spray_effects_parse() {
    for surface in [SpraySurface::Dust, SpraySurface::Snow] {
        let def = spray_effect(surface).expect("effect parses and validates");
        assert_eq!(Some(def.name.as_str()), surface.effect());
    }
    assert!(spray_effect(SpraySurface::None).is_none());
}
