// SPDX-License-Identifier: MPL-2.0
use std::time::Duration;

use regia_app::{MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_core::math::wake::WakeShape;
use regia_render::MeshRenderer;

use super::*;
use crate::camera_body::{CameraBodyPlugin, FnPath, PathSampler};
use crate::shots::{CameraPath, LookAt, PathInterp, PathKey, Shot, ShotCamera, ShotPlugin, Track};

/// A straight pass along +X at `speed`, 50 m up, passing x = 0 at t = 10 s, `lateral` metres
/// to the +Z side of the origin.
fn pass(speed: f32, lateral: f32) -> FnPath<impl Fn(f64) -> Vec3> {
    FnPath {
        position: move |t: f64| Vec3::new(speed * (t as f32 - 10.0), 50.0, lateral),
        span: (0.0, 20.0),
    }
}

/// A crow on a post at 50 m height (the camera's height), at the origin.
fn crow() -> BirdSpec {
    BirdSpec {
        name: "crow".into(),
        position: [0.0, 50.0 - 0.3 * 0.47, 0.0],
        perch: Perch::Post,
        ..BirdSpec::default()
    }
}

/// Still air: no wake anywhere.
fn still(_t: f64, _p: Vec3) -> Vec3 {
    Vec3::ZERO
}

#[test]
fn spec_json_checks_and_presets() {
    let spec =
        BirdSpec::from_json(r#"{ "name": "gull", "species": "gull", "position": [1, 2, 3] }"#).expect("spec");
    assert_eq!(spec.species, BirdSpecies::Gull);
    assert!((spec.body().wingspan_m - 1.3).abs() < 1.0e-6);
    assert!(BirdSpec::from_json(r#"{ "name": "x", "wings": 2 }"#).is_err());
    assert!(matches!(
        BirdSpec::from_json(r#"{ "name": "  " }"#),
        Err(BirdSpecError::NoName)
    ));
    assert!(matches!(
        BirdSpec::from_json(r#"{ "name": "x", "version": 7 }"#),
        Err(BirdSpecError::Version { found: 7 })
    ));
    // The flinch is held to 0.1..0.2 s.
    let quick = BirdSpec::from_json(r#"{ "name": "x", "startle": { "flinch_s": 0.5 } }"#).expect("spec");
    assert_eq!(quick.startle.flinch_s, 0.2);
    // Twice the size beats at half the rate.
    let big = BirdSpec {
        size_scale: 2.0,
        ..BirdSpec::default()
    };
    assert!((big.body().flap_hz - 2.0).abs() < 1.0e-6);
}

#[test]
fn a_fast_approach_is_seen_at_the_alert_distance() {
    let wake = WakeShape::default();
    let trigger = find_trigger(&crow(), &pass(30.0, 5.0), &wake).expect("startles");
    assert_eq!(trigger.cause, TriggerCause::Closing);
    assert!(
        (trigger.distance_m - 120.0).abs() < 0.01,
        "{}",
        trigger.distance_m
    );
    // Approaching from x = -120 (ish) at 30 m/s: about 4 s before the pass.
    assert!((trigger.time_s - (10.0 - (120.0f64.powi(2) - 25.0).sqrt() / 30.0)).abs() < 1.0e-3);
    // The same at 750 m/s: 0.16 s before the pass.
    let fast = find_trigger(&crow(), &pass(750.0, 5.0), &wake).expect("startles");
    assert!((fast.time_s - 10.0 + 0.16).abs() < 0.01, "{}", fast.time_s);
}

#[test]
fn a_supersonic_pass_out_of_sight_startles_with_its_boom() {
    let mut spec = crow();
    spec.startle.closing_speed_m_s = 0.0;
    spec.startle.distance_m = 0.0;
    let wake = WakeShape::default();
    let path = pass(750.0, 40.0);
    let trigger = find_trigger(&spec, &path, &wake).expect("the front arrives");
    assert_eq!(trigger.cause, TriggerCause::Wake);
    // On the Mach cone: the front reaches the bird at the trigger.
    let body = path.body(trigger.time_s, sim::TRIGGER_SCAN_S).expect("body");
    let since = wake.sample(&body, perch_centre(&spec)).since_front_s;
    assert!(since.abs() < 1.0e-3, "{since}");
    // After the pass: the cone trails the body.
    assert!(trigger.time_s > 10.0);
    // Too far for the wake and no visual cue: never.
    assert!(find_trigger(&spec, &pass(750.0, 400.0), &wake).is_none());
}

/// Counts the bodies [`find_trigger`] asks the path for, and where: a scan that never triggers
/// walks the span once, so the count is the scan's sample count.
struct CountingPath {
    span: (f64, f64),
    bodies: std::cell::Cell<u64>,
    last_t: std::cell::Cell<f64>,
}

impl PathSampler for CountingPath {
    fn position(&self, _time: f64) -> Option<Vec3> {
        // Never reached: `body` below does not ask the path for positions.
        Some(Vec3::new(1.0e6, 50.0, 0.0))
    }
    fn span(&self) -> (f64, f64) {
        self.span
    }
    fn body(&self, time: f64, _step: f64) -> Option<regia_core::math::wake::WakeBody> {
        self.bodies.set(self.bodies.get() + 1);
        self.last_t.set(time);
        // A camera parked a thousand kilometres away: no cue fires, so the scan walks the
        // whole span and nothing bisects.
        Some(regia_core::math::wake::WakeBody {
            position: Vec3::new(1.0e6, 50.0, 0.0),
            velocity: Vec3::ZERO,
        })
    }
}

/// A span over the scan cap takes at most [`MAX_TRIGGER_SAMPLES`] samples (the documented
/// limit, book/agents/camera-body.md) and still reaches the span's end.
#[test]
fn a_trigger_scan_takes_at_most_the_documented_samples() {
    let path = CountingPath {
        span: (0.0, 10_000.0),
        bodies: std::cell::Cell::new(0),
        last_t: std::cell::Cell::new(f64::NAN),
    };
    let trigger = find_trigger(&crow(), &path, &WakeShape::default());
    assert!(trigger.is_none(), "nothing startles a camera parked 1e6 m away");
    assert!(
        path.bodies.get() <= sim::MAX_TRIGGER_SAMPLES,
        "{} samples: the scan is over the documented cap of {}",
        path.bodies.get(),
        sim::MAX_TRIGGER_SAMPLES
    );
    assert_eq!(path.last_t.get(), 10_000.0, "the span's end is still sampled");
}

#[test]
fn phases_run_in_order_with_the_spec_timings() {
    let wake = WakeShape::default();
    let spec = crow();
    let trigger = find_trigger(&spec, &pass(30.0, 5.0), &wake);
    let sim = BirdSim::new(spec.clone(), trigger);
    let t0 = trigger.expect("trigger").time_s;
    let flinch = t0 + f64::from(spec.startle.reaction_s);
    let takeoff = flinch + f64::from(spec.startle.flinch_s);
    assert_eq!(sim.phase_at(t0 - 0.01), BirdPhase::Perched);
    assert_eq!(sim.phase_at(t0 + 0.01), BirdPhase::Alert);
    assert_eq!(sim.phase_at(flinch + 0.01), BirdPhase::Flinch);
    assert_eq!(sim.phase_at(takeoff + 0.01), BirdPhase::TakeOff);
    assert_eq!(
        sim.phase_at(takeoff + f64::from(spec.flee.takeoff_s) + 0.01),
        BirdPhase::Flee
    );
    assert_eq!(
        sim.phase_at(takeoff + f64::from(spec.flee.glide_after_s) + 0.01),
        BirdPhase::Glide
    );
    assert_eq!(BirdSim::new(spec, None).phase_at(1.0e6), BirdPhase::Perched);
}

#[test]
fn the_flinch_snaps_flares_and_widens_the_eyes() {
    let wake = WakeShape::default();
    let spec = crow();
    let trigger = find_trigger(&spec, &pass(30.0, 5.0), &wake);
    let mut sim = BirdSim::new(spec, trigger);
    let env = FlightEnv {
        gust: &still,
        floor: None,
    };
    let flinch = sim.flinch_time().expect("flinch");
    let calm = sim.pose_at(flinch - 1.0, &env);
    let mid = sim.pose_at(flinch + 0.08, &env);
    assert_eq!(calm.eye_scale, 1.0);
    assert!(mid.eye_scale > 1.5, "{}", mid.eye_scale);
    assert!(mid.wing_raise > 0.5 && mid.wing_open > 0.5, "{mid:?}");
    assert!(mid.crouch > 0.0);
    // The head points at the camera (which is to the -X side, ahead along -Z of nothing): yaw
    // turned toward -X is positive (left).
    assert!(mid.head_yaw > 0.3, "{}", mid.head_yaw);
}

#[test]
fn idle_heads_turn_and_stay_in_range() {
    let mut sim = BirdSim::new(crow(), None);
    let env = FlightEnv {
        gust: &still,
        floor: None,
    };
    let mut yaws = Vec::new();
    for i in 0..200 {
        let p = sim.pose_at(f64::from(i) * 0.05, &env);
        assert_eq!(p.phase, BirdPhase::Perched);
        assert!(p.head_yaw.abs() <= 70f32.to_radians() + 1.0e-4);
        yaws.push(p.head_yaw);
    }
    let spread = yaws.iter().copied().fold(f32::NEG_INFINITY, f32::max)
        - yaws.iter().copied().fold(f32::INFINITY, f32::min);
    assert!(spread > 0.3, "the head turns: {spread}");
}

#[test]
fn the_flight_is_deterministic_and_order_free() {
    let wake = WakeShape::default();
    let path = pass(120.0, 6.0);
    let spec = crow();
    let trigger = find_trigger(&spec, &path, &wake);
    let gust = |t: f64, p: Vec3| {
        path.body(t, 1.0 / 240.0)
            .map_or(Vec3::ZERO, |b| wake.sample(&b, p).gust)
    };
    let env = FlightEnv {
        gust: &gust,
        floor: None,
    };
    let mut a = BirdSim::new(spec.clone(), trigger);
    let mut b = BirdSim::new(spec, trigger);
    let times = [10.5, 11.0, 12.3, 14.0, 17.5];
    let forward: Vec<BirdPose> = times.iter().map(|&t| a.pose_at(t, &env)).collect();
    // Out of order, with a seek back in the middle.
    let _ = b.pose_at(17.5, &env);
    let _ = b.pose_at(11.0, &env);
    let shuffled: Vec<BirdPose> = times.iter().map(|&t| b.pose_at(t, &env)).collect();
    assert_eq!(forward, shuffled);
}

#[test]
fn it_flees_away_and_up_and_the_wake_throws_it() {
    let wake = WakeShape::default();
    let path = pass(200.0, 6.0);
    let spec = crow();
    let trigger = find_trigger(&spec, &path, &wake);
    let gust = |t: f64, p: Vec3| {
        path.body(t, 1.0 / 240.0)
            .map_or(Vec3::ZERO, |b| wake.sample(&b, p).gust)
    };
    let env = FlightEnv {
        gust: &gust,
        floor: None,
    };
    let mut sim = BirdSim::new(spec.clone(), trigger);
    let takeoff = sim.takeoff_time().expect("takeoff");
    let start = perch_centre(&spec);
    let later = sim.pose_at(takeoff + 2.0, &env);
    assert_eq!(later.phase, BirdPhase::Flee);
    assert!(later.position.y > start.y + 1.0, "climbs: {}", later.position);
    // The camera flew along z = 6; the bird breaks to the other side (-Z).
    assert!(
        later.position.z < start.z - 3.0,
        "away from the path: {}",
        later.position
    );
    assert!(later.velocity.length() > 5.0);
    // Without the wake's push the trajectory differs.
    let mut calm = BirdSim::new(
        BirdSpec {
            flee: FleeSpec {
                wake_gain: 0.0,
                ..spec.flee
            },
            ..spec
        },
        trigger,
    );
    let unpushed = calm.pose_at(takeoff + 2.0, &env);
    assert!(unpushed.position.distance(later.position) > 0.05);
    // Wings beat while fleeing, then hold still in the glide.
    let a = sim.pose_at(takeoff + 1.5, &env).wing_raise;
    let b = sim.pose_at(takeoff + 1.5 + 0.06, &env).wing_raise;
    assert!((a - b).abs() > 0.05, "flapping: {a} {b}");
    let g0 = sim.pose_at(takeoff + 5.0, &env);
    let g1 = sim.pose_at(takeoff + 5.06, &env);
    assert_eq!(g0.phase, BirdPhase::Glide);
    assert!((g0.wing_raise - g1.wing_raise).abs() < 1.0e-4);
}

#[test]
fn the_floor_keeps_it_out_of_the_ground() {
    let wake = WakeShape::default();
    let path = pass(30.0, 5.0);
    let mut spec = crow();
    spec.flee.takeoff_climb_deg = 0.0;
    spec.flee.glide_deg = 40.0;
    let trigger = find_trigger(&spec, &path, &wake);
    let floor = |_x: f32, _z: f32| Some(48.0f32);
    let env = FlightEnv {
        gust: &still,
        floor: Some(&floor),
    };
    let mut sim = BirdSim::new(spec, trigger);
    let takeoff = sim.takeoff_time().expect("takeoff");
    for i in 0..100 {
        let p = sim.pose_at(takeoff + f64::from(i) * 0.1, &env);
        assert!(p.position.y > 48.0 - 0.25, "{}", p.position);
    }
}

#[test]
fn placing_on_the_path_puts_the_pass_where_asked() {
    let path = pass(100.0, 0.0);
    let feet = place_on_path(&path, 10.0, 4.0, 0.0, 0.0).expect("placed");
    // Travelling +X: the right side is +Z.
    assert!(
        (Vec3::from_array(feet) - Vec3::new(0.0, 50.0, 4.0)).length() < 1.0e-3,
        "{feet:?}"
    );
    let spec = BirdSpec {
        position: feet,
        ..crow()
    };
    let trigger = find_trigger(&spec, &path, &WakeShape::default()).expect("startles");
    assert!(trigger.time_s < 10.0);
}

#[test]
fn the_mesh_has_every_part_and_poses_finite() {
    let spec = crow();
    let body = spec.body();
    let mesh = BirdMesh::build(&body);
    for (part, m) in BirdPart::ALL.iter().zip(&mesh.parts) {
        assert!(
            !m.vertices.is_empty() && m.indices.len().is_multiple_of(3),
            "{}",
            part.name()
        );
        assert!(
            m.vertices
                .iter()
                .all(|v| v.position.iter().all(|c| c.is_finite()))
        );
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
    }
    // The wings are mirror images.
    let (l, r) = (
        &mesh.parts[BirdPart::WingOuterLeft as usize],
        &mesh.parts[BirdPart::WingOuterRight as usize],
    );
    let max_x = |m: &regia_render::CpuMesh| {
        m.vertices
            .iter()
            .map(|v| v.position[0])
            .fold(f32::NEG_INFINITY, f32::max)
    };
    let min_x = |m: &regia_render::CpuMesh| {
        m.vertices
            .iter()
            .map(|v| v.position[0])
            .fold(f32::INFINITY, f32::min)
    };
    assert!((max_x(r) + min_x(l)).abs() < 1.0e-5);
    let mut sim = BirdSim::new(spec, None);
    let env = FlightEnv {
        gust: &still,
        floor: None,
    };
    for t in pose_parts(&body, &sim.pose_at(1.0, &env)) {
        assert!(t.translation.is_finite() && t.rotation.is_finite() && t.scale.is_finite());
    }
}

fn bird_app(sequence: ShotSequence) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ShotPlugin, CameraBodyPlugin, BirdPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    let mut player = app.world_mut().resource_mut::<ShotPlayer>();
    player.load(sequence);
    player.play();
    app
}

/// One shot along +X at 40 m/s, 50 m up, passing x = 0 at t = 5 s, 5 m to the side.
fn shot_pass() -> ShotSequence {
    ShotSequence {
        shots: vec![Shot {
            name: "pass".into(),
            duration: 10.0,
            camera: ShotCamera {
                path: CameraPath {
                    interp: PathInterp::Linear,
                    keys: vec![
                        PathKey {
                            time: 0.0,
                            position: [-200.0, 50.0, 5.0],
                        },
                        PathKey {
                            time: 10.0,
                            position: [200.0, 50.0, 5.0],
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
fn the_plugin_spawns_poses_and_reports_birds() {
    let run = || {
        let mut app = bird_app(shot_pass());
        app.world_mut().resource_mut::<BirdFlock>().spawn(crow());
        let mut phases = Vec::new();
        let mut last = None;
        for _ in 0..240 {
            app.update();
            let flock = app.world().resource::<BirdFlock>();
            if let Some(r) = flock.reports().first() {
                if phases.last() != Some(&r.phase) {
                    phases.push(r.phase);
                }
                last = Some(r.clone());
            }
        }
        let renderers = app.world_mut().query::<&MeshRenderer>().iter(app.world()).count();
        (phases, last, renderers)
    };
    let (phases, last, renderers) = run();
    assert_eq!(renderers, BIRD_PARTS);
    assert_eq!(phases.first(), Some(&BirdPhase::Perched));
    assert!(phases.contains(&BirdPhase::Flinch), "{phases:?}");
    assert!(phases.contains(&BirdPhase::Flee), "{phases:?}");
    let last = last.expect("reports");
    assert!(last.trigger.is_some());
    // Deterministic across runs.
    let again = run();
    assert_eq!(again.1, Some(last));
}

/// A camera path that counts its position samples and panics past `limit`, so an unbounded scan
/// fails instead of hanging the test.
fn counted<'a>(
    calls: &'a std::cell::Cell<u64>,
    limit: u64,
    span: (f64, f64),
    position: impl Fn(f64) -> Vec3 + 'a,
) -> FnPath<impl Fn(f64) -> Vec3 + 'a> {
    FnPath {
        position: move |t: f64| {
            calls.set(calls.get() + 1);
            assert!(
                calls.get() <= limit,
                "the trigger scan sampled the path more than {limit} times"
            );
            position(t)
        },
        span,
    }
}

/// Shot validation only asks for a finite positive duration, so a sequence may run for days (a
/// timelapse) or longer: the trigger scan is capped at `MAX_TRIGGER_SAMPLES` samples over the whole
/// span (a coarser step on a long path), not one sample per `TRIGGER_SCAN_S`. An unbounded span
/// never startles.
#[test]
fn the_trigger_scan_of_a_huge_path_is_bounded() {
    let wake = WakeShape::default();
    // Four position samples per scan sample of a million-sample cap, with slack.
    let limit = 5_000_000;
    let calls = std::cell::Cell::new(0);
    let far = |_t: f64| Vec3::new(1.0e5, 50.0, 0.0);
    assert!(find_trigger(&crow(), &counted(&calls, limit, (0.0, 1.0e9), far), &wake).is_none());
    assert!(calls.get() > 0);

    // A slow pass late in a long path is still found, at the time it happens (the distance cue
    // only: 10 m/s along +X, 5 m to the side, passing the bird at t = 9e8 s).
    calls.set(0);
    let mut spec = crow();
    spec.startle.closing_speed_m_s = 0.0;
    spec.startle.gust_m_s = 0.0;
    let late = |t: f64| Vec3::new((10.0 * (t - 9.0e8)) as f32, 50.0, 5.0);
    let trigger = find_trigger(&spec, &counted(&calls, limit, (0.0, 1.0e9), late), &wake).expect("startles");
    assert_eq!(trigger.cause, TriggerCause::Distance);
    let r = f64::from(spec.startle.distance_m);
    let expected = 9.0e8 - (r * r - 25.0).sqrt() / 10.0;
    assert!(
        (trigger.time_s - expected).abs() < 1.0e-3,
        "{} vs {expected}",
        trigger.time_s
    );

    calls.set(0);
    assert!(find_trigger(&crow(), &counted(&calls, limit, (0.0, f64::INFINITY), far), &wake).is_none());
}
