// SPDX-License-Identifier: MPL-2.0
//! Crowd animation (`docs/specs/85-retarget-crowds.md` § 3, § 6): stride matching, pose
//! sharing, LOD intervals, determinism across chunk sizes, errors and the ECS bridge.

use std::sync::Arc;

use regia_animation::demo_rig::{
    DEMO_LEGS, DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
    demo_travelling_hips,
};
use regia_animation::retarget::Retargeter;
use regia_animation::{
    AnimationClip, AnimationPlugin, CrowdAgentDesc, CrowdAgentId, CrowdAnimation, CrowdAnimationPlugin,
    CrowdAnimator, CrowdArchetype, CrowdError, CrowdLod, CrowdMember, CrowdSettings, InstanceBindPose,
    JointGlobals, SkinPalette,
};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};

fn archetype() -> CrowdArchetype {
    let sk = demo_humanoid(&DemoRig::default()).expect("rig");
    let clips = demo_locomotion(&sk, &DemoGait::standard());
    CrowdArchetype::new("plain", Arc::new(sk), clips).expect("archetype")
}

fn exact_settings() -> CrowdSettings {
    CrowdSettings {
        lods: vec![CrowdLod {
            distance: f32::INFINITY,
            interval: 1,
            phase_buckets: 0,
        }],
        chunk: 0,
        ..CrowdSettings::default()
    }
}

/// Run one agent at `velocity` for `steps`, moving it by the velocity each step, and return the
/// mean horizontal world speed (m/s) of its feet over step pairs where the gait plants the foot
/// in both steps (`feet_planted`, from the clips' contact tables), after the first cycle.
fn planted_skate(crowd: &mut CrowdAnimator, id: CrowdAgentId, velocity: Vec3, steps: usize) -> f32 {
    let dt = crowd.settings().fixed_step;
    let mut position = Vec3::ZERO;
    let mut last: Option<([Vec3; 2], [bool; 2])> = None;
    let (mut sum, mut time) = (0.0, 0.0);
    for n in 0..steps {
        position += velocity * dt;
        crowd.set_motion(id, position, velocity);
        crowd.step();
        let feet = [0, 1].map(|side| {
            crowd
                .joint_world_position(id, DEMO_LEGS[side][2])
                .expect("evaluated")
        });
        let planted = crowd.agent_state(id).expect("state").feet_planted;
        if n >= 40
            && let Some((before, was)) = last
        {
            for side in 0..2 {
                if planted[side] && was[side] {
                    let d = feet[side] - before[side];
                    sum += Vec3::new(d.x, 0.0, d.z).length();
                    time += dt;
                }
            }
        }
        last = Some((feet, planted));
    }
    assert!(time > 0.0, "no planted frames");
    sum / time
}

#[test]
fn stride_matching_keeps_planted_feet_still() {
    let base = archetype();
    let skeleton = base.skeleton.clone();
    let mut skates = Vec::new();
    for (speed, proportions, direction) in [
        (1.3, None, Vec3::NEG_Z),
        (1.3, None, Vec3::new(1.0, 0.0, 1.0).normalize()),
        (
            1.3 * 1.2,
            Some(InstanceBindPose::uniform(&skeleton, 1.2).expect("instance")),
            Vec3::X,
        ),
        (2.2, None, Vec3::NEG_Z),
    ] {
        let mut crowd = CrowdAnimator::new(exact_settings());
        let a = crowd.add_archetype(base.clone());
        let id = crowd
            .add_agent(CrowdAgentDesc {
                archetype: a,
                proportions,
                ..CrowdAgentDesc::default()
            })
            .expect("agent");
        let skate = planted_skate(&mut crowd, id, direction * speed, 160);
        let state = crowd.agent_state(id).expect("state");
        skates.push((speed, skate, state.weight, state.rate));
    }
    // Walk speed, any heading, and a 1.2× instance at its own walk speed: feet hold.
    for &(speed, skate, weight, rate) in &skates[..3] {
        assert!(
            skate < 0.1,
            "speed {speed}: skate {skate} m/s (weight {weight}, rate {rate})"
        );
        assert!((rate - 1.0).abs() < 1e-4, "speed {speed}: rate {rate}");
    }
    // Mid-blend between walk and run: the gaits' duty factors disagree at a shared phase, and
    // foot locking holds the planted foot anyway (spec 85 § 6: below 0.1 m/s).
    let (speed, skate, weight, _) = skates[3];
    assert!(weight > 0.2 && weight < 0.8, "{weight}");
    assert!(skate < 0.1, "speed {speed}: skate {skate} m/s");
}

/// One agent of `base` in an exact tier.
fn single_agent(base: CrowdArchetype) -> (CrowdAnimator, CrowdAgentId) {
    let mut crowd = CrowdAnimator::new(exact_settings());
    let a = crowd.add_archetype(base);
    let id = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            ..CrowdAgentDesc::default()
        })
        .expect("agent");
    (crowd, id)
}

/// The plain archetype with every clip passed through `shape`.
fn shaped(name: &str, shape: fn(&AnimationClip) -> AnimationClip) -> CrowdArchetype {
    let sk = demo_humanoid(&DemoRig::default()).expect("rig");
    let clips = demo_locomotion(&sk, &DemoGait::standard())
        .iter()
        .map(shape)
        .collect();
    CrowdArchetype::new(name, Arc::new(sk), clips).expect("archetype")
}

fn assert_standard_speeds(base: &CrowdArchetype) {
    let speeds: Vec<(&str, f32)> = base
        .gaits()
        .iter()
        .map(|g| (g.clip.name.as_str(), g.speed))
        .collect();
    assert_eq!(speeds[0], ("idle", 0.0), "{speeds:?}");
    assert_eq!(speeds[1].0, "walk", "{speeds:?}");
    assert_eq!(speeds[2].0, "run", "{speeds:?}");
    assert!((speeds[1].1 - 1.3).abs() < 0.04, "walk {speeds:?}");
    assert!((speeds[2].1 - 3.2).abs() < 0.1, "run {speeds:?}");
    assert!(
        (base.forward() - Vec3::NEG_Z).length() < 0.02,
        "{}",
        base.forward()
    );
}

/// Imported clips have no root-motion track (the importer's default): the ground speed of a
/// treadmill clip comes from the stance foot, stride matching and foot locking still work.
#[test]
fn in_place_clips_get_their_speed_from_the_stance_foot() {
    let base = shaped("in-place", demo_in_place);
    assert_standard_speeds(&base);
    for speed in [1.3f32, 2.2] {
        let (mut crowd, id) = single_agent(base.clone());
        let skate = planted_skate(&mut crowd, id, Vec3::NEG_Z * speed, 160);
        assert!(skate < 0.1, "speed {speed}: skate {skate} m/s");
    }
}

/// A clip that carries its travel on the hips, with no root-motion track, is pinned in place: the
/// rig stays on the agent and the speed is the travel, in meters also under a scaled root.
#[test]
fn travelling_hips_are_found_and_pinned_in_place() {
    let base = shaped("hips", demo_travelling_hips);
    assert_standard_speeds(&base);
    let (mut crowd, id) = single_agent(base.clone());
    let skate = planted_skate(&mut crowd, id, Vec3::NEG_Z * 1.3, 160);
    assert!(skate < 0.1, "skate {skate} m/s");
    // 160 steps of 1.3 m/s are 6.9 m: the hips stay on the agent.
    let world = crowd.world_transform(id).expect("world");
    let hips = crowd.joint_world_position(id, 1).expect("hips");
    let agent = world.w_axis.truncate();
    let off = Vec3::new(hips.x - agent.x, 0.0, hips.z - agent.z).length();
    assert!(off < 0.05, "hips {off} m from the agent");

    // Centimetre rig: the hips' local translations are in centimetres under a 0.01 root scale.
    let source = demo_humanoid(&DemoRig::default()).expect("source");
    let clips: Vec<AnimationClip> = demo_locomotion(&source, &DemoGait::standard())
        .iter()
        .map(demo_travelling_hips)
        .collect();
    let target = demo_humanoid(&DemoRig {
        naming: RigNaming::Mixamo,
        centimetres: true,
        ..DemoRig::default()
    })
    .expect("target");
    let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
    let cm = CrowdArchetype::retargeted("cm", &rt, &clips).expect("archetype");
    assert_standard_speeds(&cm);
    let (mut crowd, id) = single_agent(cm);
    let skate = planted_skate(&mut crowd, id, Vec3::NEG_Z * 1.3, 160);
    assert!(skate < 0.1, "cm rig: skate {skate} m/s");
}

/// A rig whose legs the guesser cannot find takes its speeds from the caller.
#[test]
fn explicit_gait_speeds_order_the_gaits_and_are_checked() {
    let sk = demo_humanoid(&DemoRig::default()).expect("rig");
    let mut legless = sk.clone();
    for name in &mut legless.joint_names {
        *name = name.replace("left", "a").replace("right", "b");
    }
    let clips: Vec<AnimationClip> = demo_locomotion(&sk, &DemoGait::standard())
        .iter()
        .map(demo_in_place)
        .collect();
    let base = CrowdArchetype::new("legless", Arc::new(legless), clips).expect("archetype");
    assert!(!base.has_legs());
    assert!(base.gaits().iter().all(|g| g.speed == 0.0), "nothing to measure");
    let base = base
        .with_gait_speed("run", 3.0)
        .and_then(|b| b.with_gait_speed("walk", 1.2))
        .expect("speeds");
    let order: Vec<&str> = base.gaits().iter().map(|g| g.clip.name.as_str()).collect();
    assert_eq!(order, ["idle", "walk", "run"]);
    let (mut crowd, id) = single_agent(base.clone());
    crowd.set_motion(id, Vec3::ZERO, Vec3::NEG_Z * 2.1);
    crowd.step();
    let state = crowd.agent_state(id).expect("state");
    assert_eq!(state.gait, 1);
    assert!((state.weight - 0.5).abs() < 1e-4, "{state:?}");

    let missing = base.clone().with_gait_speed("swim", 1.0).expect_err("unknown");
    assert_eq!(missing, CrowdError::UnknownClip("swim".into()));
    assert_eq!(missing.code(), "unknown_clip");
    for bad in [f32::NAN, f32::INFINITY, -1.0] {
        let error = base.clone().with_gait_speed("walk", bad).expect_err("bad");
        assert_eq!(error, CrowdError::BadValue("speed"));
    }
}

/// Planted-foot skate of one agent of the plain archetype at `speed` along −Z.
fn skate_at(speed: f32, foot_lock: bool) -> (f32, CrowdAnimator, CrowdAgentId) {
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        foot_lock,
        ..exact_settings()
    });
    let a = crowd.add_archetype(archetype());
    let id = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            ..CrowdAgentDesc::default()
        })
        .expect("agent");
    let skate = planted_skate(&mut crowd, id, Vec3::NEG_Z * speed, 160);
    (skate, crowd, id)
}

#[test]
fn foot_locking_holds_planted_feet_across_gait_blends() {
    let base = archetype();
    assert!(base.has_legs());
    for speed in [0.7f32, 1.0, 1.7, 2.2, 2.8] {
        let (locked, crowd, id) = skate_at(speed, true);
        let (free, _, _) = skate_at(speed, false);
        println!("speed {speed}: skate locked {locked:.4} m/s, unlocked {free:.4} m/s");
        assert!(locked < 0.1, "speed {speed}: locked skate {locked} m/s");
        assert!(
            locked <= free + 1e-4,
            "speed {speed}: locking made it worse ({locked} > {free})"
        );
        let state = crowd.agent_state(id).expect("state");
        assert!(
            state.weight > 0.0 && state.weight < 1.0,
            "speed {speed} is a blend"
        );
        assert!(crowd.stats().locked_feet <= 2);
    }
    // Without locking, the walk/run blend visibly slides: the lock is what holds the foot.
    let (free, _, _) = skate_at(2.2, false);
    assert!(free > 0.2, "unlocked mid-blend skate {free} m/s");
}

#[test]
fn feet_lock_while_planted_and_only_in_exact_tiers() {
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        chunk: 0,
        ..CrowdSettings::default()
    });
    let a = crowd.add_archetype(archetype());
    let near = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            position: Vec3::new(2.0, 0.0, 0.0),
            ..CrowdAgentDesc::default()
        })
        .expect("near");
    let far = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            position: Vec3::new(30.0, 0.0, 0.0),
            ..CrowdAgentDesc::default()
        })
        .expect("far");
    crowd.set_viewer(Some(Vec3::ZERO));
    let dt = crowd.settings().fixed_step;
    let velocity = Vec3::NEG_Z * 1.3;
    let (mut seen_one, mut seen_two) = (false, false);
    for n in 1..=90 {
        for (id, x) in [(near, 2.0), (far, 30.0)] {
            crowd.set_motion(id, Vec3::new(x, 0.0, 0.0) + velocity * dt * n as f32, velocity);
        }
        crowd.step();
        let near_feet = crowd.agent_state(near).expect("near").feet_locked;
        let far_feet = crowd.agent_state(far).expect("far").feet_locked;
        assert_eq!(far_feet, [false, false], "a shared tier never locks");
        let count = near_feet.iter().filter(|&&f| f).count();
        assert_eq!(crowd.stats().locked_feet, count);
        // A walk always has a foot down (or fading out): at least one foot is held.
        assert!(count >= 1, "step {n}: no foot held");
        seen_one |= count == 1;
        seen_two |= count == 2;
    }
    assert!(seen_one && seen_two, "single and double support both occur");
    // Turning the lock off releases every foot at the next step.
    let mut settings = crowd.settings().clone();
    settings.foot_lock = false;
    crowd.set_settings(settings);
    crowd.step();
    assert_eq!(crowd.stats().locked_feet, 0);
}

#[test]
fn far_agents_share_poses_and_update_on_their_interval() {
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        chunk: 0,
        ..CrowdSettings::default()
    });
    let a = crowd.add_archetype(archetype());
    // 30 agents per tier: near (5 m), mid (25 m), far (100 m).
    for (k, distance) in [5.0f32, 25.0, 100.0].into_iter().enumerate() {
        for i in 0..30 {
            crowd
                .add_agent(CrowdAgentDesc {
                    archetype: a,
                    position: Vec3::new(distance, 0.0, i as f32 * 0.1),
                    velocity: Vec3::new(0.0, 0.0, -1.0),
                    phase: (i * 7 + k) as f32 * 0.013,
                    ..CrowdAgentDesc::default()
                })
                .expect("agent");
        }
    }
    crowd.set_viewer(Some(Vec3::ZERO));
    crowd.step();
    let first = crowd.stats().clone();
    assert_eq!(first.updated, 90, "everyone is evaluated once");
    assert_eq!(first.per_tier, vec![30, 30, 30]);
    assert!(first.shared_hits > 0, "{first:?}");
    assert!(first.evaluated_poses < 90, "{first:?}");
    for _ in 0..7 {
        let step = crowd.stats().step;
        crowd.step();
        let stats = crowd.stats();
        // Tier intervals 1, 2, 4 with the id stagger.
        let due = |base: usize, interval: u64| {
            (0..30)
                .filter(|i| (step + (base + i) as u64).is_multiple_of(interval))
                .count()
        };
        assert_eq!(stats.updated, 30 + due(30, 2) + due(60, 4), "step {step}");
        assert_eq!(stats.updated + stats.skipped, 90);
    }
    // Every agent has a palette of the skeleton's size.
    for id in 0..90 {
        let palette = crowd.palette(CrowdAgentId(id)).expect("palette");
        assert_eq!(palette.len(), 20);
    }
}

fn mixed_crowd(chunk: usize) -> CrowdAnimator {
    let source = demo_humanoid(&DemoRig::default()).expect("source");
    let clips = demo_locomotion(&source, &DemoGait::standard());
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        chunk,
        ..CrowdSettings::default()
    });
    for rig in [
        DemoRig {
            naming: RigNaming::Mixamo,
            centimetres: true,
            ..DemoRig::default()
        },
        DemoRig {
            naming: RigNaming::Game,
            facing_positive_z: true,
            rolled_frames: true,
            extra_joints: true,
            a_pose: true,
            size: 0.95,
            ..DemoRig::default()
        },
    ] {
        let target = demo_humanoid(&rig).expect("target");
        let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
        crowd.add_archetype(
            CrowdArchetype::retargeted(format!("{:?}", rig.naming), &rt, &clips).expect("archetype"),
        );
    }
    for i in 0..120u32 {
        let archetype = (i % 2) as usize;
        let skeleton = crowd.archetypes()[archetype].skeleton.clone();
        let proportions = i
            .is_multiple_of(5)
            .then(|| InstanceBindPose::uniform(&skeleton, 0.9 + 0.05 * (i % 4) as f32))
            .transpose()
            .expect("instance");
        crowd
            .add_agent(CrowdAgentDesc {
                archetype,
                position: Vec3::new((i % 12) as f32 * 4.0, 0.0, (i / 12) as f32 * 4.0),
                velocity: Vec3::new(0.3 * (i % 3) as f32, 0.0, -((i % 4) as f32)),
                phase: (i as f32 * 0.37).fract(),
                proportions,
                ..CrowdAgentDesc::default()
            })
            .expect("agent");
    }
    crowd.set_viewer(Some(Vec3::new(20.0, 1.7, 20.0)));
    crowd
}

#[test]
fn output_is_bit_identical_for_every_chunk_size() {
    let mut digests = Vec::new();
    for chunk in [0usize, 1, 7, 64] {
        let mut crowd = mixed_crowd(chunk);
        for _ in 0..45 {
            crowd.step();
        }
        digests.push((chunk, crowd.digest(), crowd.stats().clone()));
    }
    for (chunk, digest, stats) in &digests[1..] {
        assert_eq!(*digest, digests[0].1, "chunk {chunk}");
        assert_eq!(*stats, digests[0].2, "chunk {chunk}");
    }
    // `advance` runs the same steps as `step` (the remainder carries).
    let mut a = mixed_crowd(8);
    let mut b = mixed_crowd(8);
    for _ in 0..10 {
        a.step();
    }
    let mut ran = 0;
    for _ in 0..20 {
        ran += b.advance(1.0 / 60.0);
    }
    assert_eq!(ran, 10);
    assert_eq!(a.digest(), b.digest());
}

/// ADR-4477 / audit animation1-determinism H5: the diagonal facing and the world matrix use
/// portable trig; these quaternion bits differ by one ULP from glibc's half-angle sine.
#[test]
fn crowd_facing_uses_the_portable_yaw_oracle() {
    let half_yaw = f32::from_bits(0x3edf_285b);
    let yaw = 2.0 * half_yaw;
    let (sin, cos) = yaw.sin_cos();
    let (mut crowd, id) = single_agent(archetype());
    crowd.set_motion(id, Vec3::ZERO, Vec3::new(-sin, 0.0, -cos) * 1.3);
    crowd.step();
    let state = crowd.agent_state(id).expect("state");
    assert_eq!(state.yaw.to_bits(), yaw.to_bits());
    let expected = Mat4::from_rotation_translation(
        Quat::from_xyzw(0.0, f32::from_bits(0x3ed8_28b0), 0.0, f32::from_bits(0x3f68_1102)),
        Vec3::ZERO,
    );
    let actual = crowd.world_transform(id).expect("world");
    assert_eq!(
        actual.to_cols_array().map(f32::to_bits),
        expected.to_cols_array().map(f32::to_bits)
    );
}

#[test]
fn bad_agents_and_archetypes_are_refused_with_codes() {
    let sk = Arc::new(demo_humanoid(&DemoRig::default()).expect("rig"));
    assert_eq!(
        CrowdArchetype::new("none", sk.clone(), Vec::new())
            .err()
            .map(|e| e.code()),
        Some("no_gaits")
    );
    let mut bad = demo_locomotion(&sk, &DemoGait::standard());
    bad[0].duration = f32::NAN;
    assert!(matches!(
        CrowdArchetype::new("bad", sk.clone(), bad),
        Err(CrowdError::BadClip(_))
    ));
    let mut crowd = CrowdAnimator::new(exact_settings());
    assert_eq!(
        crowd.add_agent(CrowdAgentDesc::default()).err().map(|e| e.code()),
        Some("unknown_archetype")
    );
    let a = crowd.add_archetype(archetype());
    let other = demo_humanoid(&DemoRig {
        extra_joints: true,
        ..DemoRig::default()
    })
    .expect("rig");
    let err = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            proportions: Some(InstanceBindPose::uniform(&other, 1.0).expect("instance")),
            ..CrowdAgentDesc::default()
        })
        .expect_err("mismatch");
    assert_eq!(err.code(), "proportions_mismatch");
    // The public fields of a hand-built instance can disagree with its bind pose.
    let mut short =
        InstanceBindPose::uniform(&demo_humanoid(&DemoRig::default()).expect("rig"), 1.0).expect("instance");
    short.inverse_bind_matrices.truncate(3);
    let err = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            proportions: Some(short),
            ..CrowdAgentDesc::default()
        })
        .expect_err("short inverse binds");
    assert_eq!(err.code(), "proportions_mismatch");
    let err = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            velocity: Vec3::new(f32::NAN, 0.0, 0.0),
            ..CrowdAgentDesc::default()
        })
        .expect_err("nan");
    assert_eq!(err.code(), "bad_value");
    // Non-finite motion updates are ignored; unknown agents report false.
    let id = crowd
        .add_agent(CrowdAgentDesc {
            archetype: a,
            ..CrowdAgentDesc::default()
        })
        .expect("agent");
    assert!(crowd.set_motion(id, Vec3::splat(f32::INFINITY), Vec3::ONE));
    assert_eq!(crowd.agent_state(id).map(|s| s.position), Some(Vec3::ZERO));
    assert!(!crowd.set_motion(CrowdAgentId(99), Vec3::ZERO, Vec3::ZERO));
    assert!(
        crowd.palette(id).is_none(),
        "nothing evaluated before the first step"
    );
}

#[test]
fn the_plugin_steps_the_crowd_and_writes_member_palettes() {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin, CrowdAnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(40),
    ));
    let base = archetype();
    let skeleton = base.skeleton.clone();
    let id = {
        let mut crowd = app.world_mut().resource_mut::<CrowdAnimation>();
        let a = crowd.0.add_archetype(base);
        crowd
            .0
            .add_agent(CrowdAgentDesc {
                archetype: a,
                velocity: Vec3::new(1.3, 0.0, 0.0),
                ..CrowdAgentDesc::default()
            })
            .expect("agent")
    };
    let binding = regia_animation::components::spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((
            regia_scene::Transform::default(),
            binding,
            CrowdMember {
                agent: id,
                drive_transform: true,
            },
        ))
        .id();
    for _ in 0..10 {
        app.update();
    }
    let steps = app.world().resource::<CrowdAnimation>().0.stats().step;
    assert!(steps > 0, "FixedUpdate stepped the crowd");
    let palette = app.world().get::<SkinPalette>(rig).expect("palette written");
    assert_eq!(palette.matrices.len(), 20);
    // Spec 85 § 3.1: the plugin also writes the member's joint globals, equal to the crowd's.
    let globals = app
        .world()
        .get::<JointGlobals>(rig)
        .expect("joint globals written");
    let crowd_globals = app
        .world()
        .resource::<CrowdAnimation>()
        .0
        .globals(id)
        .expect("agent globals")
        .to_vec();
    assert_eq!(globals.globals, crowd_globals);
    assert_eq!(globals.globals.len(), 20);
    let yaw = app
        .world()
        .resource::<CrowdAnimation>()
        .0
        .agent_state(id)
        .expect("state")
        .yaw;
    let rotation = app
        .world()
        .get::<regia_scene::Transform>(rig)
        .expect("transform")
        .rotation;
    assert!((rotation.to_euler(regia_core::glam::EulerRot::YXZ).0 - yaw).abs() < 1e-4);
    // The rig faces its velocity (+X): the archetype's forward (−Z) turned by the yaw.
    let forward = rotation * Vec3::NEG_Z;
    assert!((forward - Vec3::X).length() < 1e-3, "{forward}");
}

/// Hand-run timing (`cargo test -p regia_animation --release --test crowd -- --ignored --nocapture`):
/// prints the mean step time of 500 agents with sharing on and off, serial and chunked, on the
/// plain rig, the rig with twist and IK joints, and the 60-joint budget rig. No wall-clock assert (spec 85 § 8 budgets
/// are checked by eye and recorded in the lane report).
#[test]
#[ignore = "hand-run timing"]
fn crowd_step_timing() {
    let extra = demo_humanoid(&DemoRig {
        extra_joints: true,
        ..DemoRig::default()
    })
    .expect("rig");
    let extra_clips = demo_locomotion(&extra, &DemoGait::standard());
    // The original 20/27-joint cases do not measure spec 85's 60-joint workload. Retain
    // them and add a full-size palette with accessory joints parented to the animated spine.
    let mut names = extra.joint_names.clone();
    let mut parents = extra.parents.clone();
    let mut binds = extra.bind_pose.clone();
    while names.len() < 60 {
        names.push(format!("accessory_{}", names.len()));
        parents.push(Some(2));
        binds.push(regia_animation::skeleton::LocalBind::default());
    }
    let budget = regia_animation::Skeleton::from_local_bind(names, parents, binds).expect("60-joint rig");
    let budget_clips = demo_locomotion(&budget, &DemoGait::standard());
    let rigs = [
        archetype(),
        CrowdArchetype::new("extra", Arc::new(extra), extra_clips).expect("archetype"),
        CrowdArchetype::new("budget", Arc::new(budget), budget_clips).expect("budget archetype"),
    ];
    for base in &rigs {
        let joints = base.skeleton.len();
        for (label, lods, chunk) in [
            ("shared tiers, serial", CrowdSettings::default().lods, 0usize),
            ("shared tiers, chunk 32", CrowdSettings::default().lods, 32),
            ("exact, serial", exact_settings().lods, 0),
            ("exact, chunk 32", exact_settings().lods, 32),
        ] {
            let mut crowd = CrowdAnimator::new(CrowdSettings {
                lods,
                chunk,
                ..CrowdSettings::default()
            });
            let a = crowd.add_archetype(base.clone());
            for i in 0..500u32 {
                crowd
                    .add_agent(CrowdAgentDesc {
                        archetype: a,
                        position: Vec3::new((i % 25) as f32 * 3.0, 0.0, (i / 25) as f32 * 3.0),
                        velocity: Vec3::new(0.0, 0.0, -0.5 - (i % 7) as f32 * 0.4),
                        phase: (i as f32 * 0.137).fract(),
                        ..CrowdAgentDesc::default()
                    })
                    .expect("agent");
            }
            crowd.set_viewer(Some(Vec3::new(36.0, 1.7, -5.0)));
            for _ in 0..10 {
                crowd.step();
            }
            let steps = 200;
            let start = std::time::Instant::now();
            let mut evaluated = 0;
            for _ in 0..steps {
                crowd.step();
                evaluated += crowd.stats().evaluated_poses;
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(steps);
            println!(
                "crowd_step_timing: {label}: {ms:.3} ms/step, {:.0} poses/step, 500 agents x {joints} joints",
                evaluated as f64 / f64::from(steps)
            );
        }
    }
}

fn fill_crowd(settings: CrowdSettings, agents: u32, phase_step: f32) -> CrowdAnimator {
    let mut crowd = CrowdAnimator::new(settings);
    let archetype = crowd.add_archetype(archetype());
    for index in 0..agents {
        crowd
            .add_agent(CrowdAgentDesc {
                archetype,
                velocity: Vec3::new(0.0, 0.0, 1.2),
                phase: (index as f32 * phase_step).fract(),
                ..CrowdAgentDesc::default()
            })
            .expect("agent");
    }
    crowd
}

#[test]
fn exact_tier_step_digest_is_stable_with_a_reused_key_map() {
    let mut crowd = fill_crowd(exact_settings(), 64, 0.0);
    let mut other = fill_crowd(exact_settings(), 64, 0.0);
    let mut cap_at_2 = None;
    for step in 0..8 {
        crowd.step();
        other.step();
        assert_eq!(crowd.digest(), other.digest());
        assert_eq!(crowd.stats().evaluated_poses, 64);
        assert_eq!(crowd.key_map_capacity(), 0);
        if step == 1 {
            cap_at_2 = Some(crowd.key_map_capacity());
        }
    }
    assert_eq!(crowd.key_map_capacity(), cap_at_2.expect("step 2"));

    let mut shared = CrowdAnimator::new(CrowdSettings {
        lods: vec![CrowdLod {
            distance: f32::INFINITY,
            interval: 1,
            phase_buckets: 8,
        }],
        chunk: 0,
        ..CrowdSettings::default()
    });
    let archetype = shared.add_archetype(archetype());
    for _ in 0..64 {
        shared
            .add_agent(CrowdAgentDesc {
                archetype,
                velocity: Vec3::new(0.0, 0.0, 1.2),
                phase: 0.0,
                ..CrowdAgentDesc::default()
            })
            .expect("agent");
    }
    let mut shared_cap = None;
    for step in 0..8 {
        shared.step();
        assert!(shared.stats().evaluated_poses < 64);
        assert!(shared.stats().shared_hits > 0);
        assert!(shared.key_map_capacity() > 0);
        if step == 1 {
            shared_cap = Some(shared.key_map_capacity());
        }
    }
    assert_eq!(shared.key_map_capacity(), shared_cap.expect("shared step 2"));
}

/// Spec 85 § 3 step 3 and § 5: agents with one pose key share one pose, evaluated at the bucket's
/// value, never at the first agent's own phase. Two agents in one phase bucket give the same
/// palette, and the shared palette does not depend on which of them was added first.
#[test]
fn a_shared_pose_is_evaluated_at_the_bucket_value() {
    const BUCKETS: u32 = 16;
    let settings = || CrowdSettings {
        chunk: 0,
        foot_lock: false,
        lods: vec![CrowdLod {
            distance: f32::INFINITY,
            interval: 1,
            phase_buckets: BUCKETS,
        }],
        ..CrowdSettings::default()
    };
    let velocity = Vec3::new(0.0, 0.0, -1.3);
    let desc = |phase: f32| CrowdAgentDesc {
        archetype: 0,
        velocity,
        phase,
        ..CrowdAgentDesc::default()
    };
    // How far one step moves the phase, so both agents land inside one bucket after it.
    let mut probe = CrowdAnimator::new(settings());
    probe.add_archetype(archetype());
    let p = probe.add_agent(desc(0.0)).expect("probe");
    probe.step();
    let advance = probe.agent_state(p).expect("state").phase;
    assert!(advance > 0.0 && advance < 0.5, "{advance}");
    // Phases quantize to the nearest bucket value (`round(φ · buckets)`): put both agents a
    // fifth of a bucket either side of bucket 5's value after the step.
    let bucket = 1.0 / BUCKETS as f32;
    let start = |at: f32| (5.0 * bucket + at * bucket - advance).rem_euclid(1.0);
    let (early, late) = (start(-0.2), start(0.2));

    let run = |order: [f32; 2]| {
        let mut crowd = CrowdAnimator::new(settings());
        crowd.add_archetype(archetype());
        let ids = order.map(|phase| crowd.add_agent(desc(phase)).expect("agent"));
        crowd.step();
        let stats = crowd.stats().clone();
        assert_eq!(stats.evaluated_poses, 1, "one key, one pose: {stats:?}");
        assert_eq!(stats.shared_hits, 1, "{stats:?}");
        let palettes = ids.map(|id| crowd.palette(id).expect("palette").to_vec());
        assert_eq!(palettes[0], palettes[1], "same key, same palette");
        let phases = ids.map(|id| crowd.agent_state(id).expect("state").phase);
        assert_ne!(phases[0], phases[1], "the agents keep their own phases");
        palettes[0].clone()
    };
    let early_first = run([early, late]);
    let late_first = run([late, early]);
    assert_eq!(
        early_first, late_first,
        "the shared pose is the bucket's, not the first agent's"
    );
}

/// Spec 85 § 3 step 1: facing comes from the velocity only above 0.05 m/s; at or below that
/// speed the agent keeps the yaw it had.
#[test]
fn facing_is_kept_at_or_below_the_speed_threshold() {
    let (mut crowd, id) = single_agent(archetype());
    crowd.set_motion(id, Vec3::ZERO, Vec3::new(1.3, 0.0, 0.0));
    crowd.step();
    let turned = crowd.agent_state(id).expect("state").yaw;
    assert!(turned.abs() > 0.1, "a +X walk turns the agent: {turned}");
    for slow in [Vec3::new(0.0, 0.0, 0.04), Vec3::new(-0.03, 0.0, 0.03), Vec3::ZERO] {
        assert!(crowd.set_motion(id, Vec3::ZERO, slow));
        crowd.step();
        let state = crowd.agent_state(id).expect("state");
        assert_eq!(state.yaw.to_bits(), turned.to_bits(), "{slow}");
    }
    crowd.set_motion(id, Vec3::ZERO, Vec3::new(0.0, 0.0, 0.06));
    crowd.step();
    assert_ne!(
        crowd.agent_state(id).expect("state").yaw.to_bits(),
        turned.to_bits()
    );
}
