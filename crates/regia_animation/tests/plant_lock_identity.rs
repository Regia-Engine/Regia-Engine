// SPDX-License-Identifier: MPL-2.0
//! One foot-lock state machine (ADR-8308): the planter, the motion-matching `FootLocker` and the
//! crowd's exact-tier lock run on `foot_ik::lock::PlantLock` and give the bits they gave before.
//!
//! The planter and crowd digests were recorded on the base before the refactor (the lane report has
//! the run) and are pinned here; the motion-matching locker is compared, step by step and bit by
//! bit, against a frozen copy of its pre-refactor update and against a bare `PlantLock`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use regia_animation::demo_rig::{DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use regia_animation::foot_ik::lock::{PlantLock, PlantRelease};
use regia_animation::foot_ik::{FootGround, FootPlantInput, FootPlantLeg, FootPlantSettings, FootPlanter};
use regia_animation::motion_matching::{FootLock, FootLockSettings, FootLocker};
use regia_animation::{CrowdAgentDesc, CrowdAnimator, CrowdArchetype, CrowdLod, CrowdSettings};
use regia_core::math::{Quat, Vec3};

fn fnv(hash: &mut u64, bits: u32) {
    for byte in bits.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn fnv_vec(hash: &mut u64, v: Vec3) {
    for c in v.to_array() {
        fnv(hash, c.to_bits());
    }
}

/// A small xorshift for deterministic test inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Two legs over 900 frames: stances, swings, a shuffle (a low fast foot), a teleport, a lock
/// switched off and on, a release with zero time, a rotated rig, a non-finite input and an
/// airborne stretch. Hashes every output bit and the pins.
fn planter_digest() -> u64 {
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let dt = 1.0 / 60.0;
    for frame in 0..900u32 {
        let t = frame as f32 * dt;
        if frame == 300 {
            planter.settings.lock = false;
        }
        if frame == 360 {
            planter.settings.lock = true;
            planter.settings.lock_release_time = 0.0;
        }
        if frame == 480 {
            planter.settings.lock_release_time = 0.15;
        }
        let jump = if frame >= 420 { 3.0 } else { 0.0 };
        let rig = Vec3::new(jump, 0.0, -1.2 * t);
        let yaw = if frame >= 600 { 0.7 } else { 0.0 };
        let rotation = Quat::from_rotation_y(yaw);
        let legs: Vec<FootPlantLeg> = [0.0f32, 0.5]
            .iter()
            .enumerate()
            .map(|(i, &offset)| {
                let phase = (t + offset).fract();
                let side = if i == 0 { 1.0 } else { -1.0 };
                // Stance for the first 60 %, the foot sliding back at 1.1 m/s; then a swing.
                let (z, lift) = if phase < 0.6 {
                    (0.33 - 1.1 * phase, 0.0)
                } else {
                    let s = (phase - 0.6) / 0.4;
                    (-0.33 + 0.66 * s, 0.12 * (std::f32::consts::PI * s).sin())
                };
                // A shuffle: frames 200..220 keep the foot low but fast.
                let lift = if (200..220).contains(&frame) { 0.0 } else { lift };
                let local = Vec3::new(0.1 * side, 0.1 + lift, z);
                let foot = if frame == 700 && i == 1 {
                    Vec3::NAN
                } else {
                    rotation * local + rig
                };
                FootPlantLeg {
                    hip: rotation * Vec3::new(0.1 * side, 0.9, 0.0) + rig,
                    foot,
                    length: 0.82,
                    ankle_height: 0.1,
                    ground: FootGround::Contact(Vec3::new(foot.x, 0.02 * (t * 3.0).sin(), foot.z)),
                    normal: Vec3::new(0.1, 1.0, 0.05).normalize(),
                    contact: None,
                }
            })
            .collect();
        let plan = planter.plan(&FootPlantInput {
            rig_translation: rig,
            rig_rotation: rotation,
            legs: &legs,
            active: !(800..830).contains(&frame),
            dt,
        });
        fnv(&mut hash, plan.weight.to_bits());
        fnv(&mut hash, plan.pelvis_offset.to_bits());
        for (i, leg) in plan.legs.iter().enumerate() {
            fnv_vec(&mut hash, leg.target);
            for c in leg.tilt.to_array() {
                fnv(&mut hash, c.to_bits());
            }
            fnv(&mut hash, leg.planted.to_bits());
            fnv(&mut hash, leg.lock.to_bits());
            fnv(&mut hash, planter.slide(i).to_bits());
            fnv_vec(&mut hash, planter.pin(i).unwrap_or(Vec3::splat(-7.0)));
        }
    }
    hash
}

/// The planter's output over the scripted walk, pinned from the base before the refactor.
#[test]
fn foot_planter_output_is_bit_identical_to_the_base() {
    let digest = planter_digest();
    println!("planter digest {digest:#018x}");
    assert_eq!(digest, planter_digest(), "deterministic");
    assert_eq!(digest, PLANTER_DIGEST, "the planter's bits changed");
}

/// A walk/run crowd in an exact tier with foot locking, at speeds that blend gaits, change speed and
/// teleport: the crowd digest over every step, pinned from the base before the refactor.
fn crowd_digest() -> (u64, usize) {
    let sk = demo_humanoid(&DemoRig::default()).expect("rig");
    let clips = demo_locomotion(&sk, &DemoGait::standard());
    let archetype = CrowdArchetype::new("plain", Arc::new(sk), clips).expect("archetype");
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        lods: vec![CrowdLod {
            distance: f32::INFINITY,
            interval: 1,
            phase_buckets: 0,
        }],
        chunk: 0,
        ..CrowdSettings::default()
    });
    let a = crowd.add_archetype(archetype);
    let mut agents: Vec<(regia_animation::CrowdAgentId, Vec3, Vec3)> = (0..6)
        .map(|k| {
            let velocity = Vec3::new(0.0, 0.0, -(0.6 + 0.45 * k as f32));
            let id = crowd
                .add_agent(CrowdAgentDesc {
                    archetype: a,
                    velocity,
                    phase: k as f32 * 0.17,
                    ..CrowdAgentDesc::default()
                })
                .expect("agent");
            (id, Vec3::new(k as f32, 0.0, 0.0), velocity)
        })
        .collect();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut locked = 0usize;
    for step in 0..240u32 {
        for (k, (id, position, velocity)) in agents.iter_mut().enumerate() {
            if step == 100 {
                *velocity = Vec3::new(0.3, 0.0, -(2.6 - 0.4 * k as f32));
            }
            *position += *velocity / 60.0;
            if step == 160 && k == 2 {
                *position = Vec3::new(5.0, 0.0, 5.0);
            }
            assert!(crowd.set_motion(*id, *position, *velocity));
        }
        crowd.step();
        let d = crowd.digest();
        fnv(&mut hash, d as u32);
        fnv(&mut hash, (d >> 32) as u32);
        locked += crowd.stats().locked_feet;
    }
    (hash, locked)
}

/// The crowd's exact-tier foot lock, pinned from the base before the refactor.
#[test]
fn crowd_foot_lock_is_bit_identical_to_the_base() {
    let (digest, locked) = crowd_digest();
    println!("crowd digest {digest:#018x}, locked feet over the run {locked}");
    assert!(locked > 0, "the scenario locks feet");
    assert_eq!((digest, locked), crowd_digest(), "deterministic");
    assert_eq!(
        (digest, locked),
        (CROWD_DIGEST, CROWD_LOCKED),
        "the crowd's bits changed"
    );
}

/// The motion-matching locker's update before it became a `PlantLock` wrapper, verbatim in
/// behavior (`motion_matching/contacts.rs` at the lane base).
#[derive(Default)]
struct FrozenLocker {
    settings: FootLockSettings,
    locks: Vec<FootLock>,
    pinned: Vec<bool>,
}

impl FrozenLocker {
    fn update(&mut self, planted: &[bool], animated: &[Vec3], dt: f32) -> &[FootLock] {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let feet = animated.len();
        self.locks.resize(feet, FootLock::default());
        self.pinned.resize(feet, false);
        let release = if self.settings.release_time.is_finite() {
            self.settings.release_time.max(0.0)
        } else {
            0.0
        };
        let drift = if self.settings.max_drift.is_finite() {
            self.settings.max_drift.max(0.0)
        } else {
            f32::INFINITY
        };
        let feet_state = self.locks.iter_mut().zip(self.pinned.iter_mut()).zip(animated);
        for (foot, ((lock, pinned), &now)) in feet_state.enumerate() {
            let down = planted.get(foot).copied().unwrap_or(false) && now.is_finite();
            let touchdown = down && !lock.planted;
            lock.planted = down;
            if down {
                if touchdown || lock.target.distance(now) > drift {
                    lock.target = now;
                }
                *pinned = true;
                lock.weight = 1.0;
            } else if *pinned {
                lock.weight = if release > 0.0 {
                    (lock.weight - dt / release).max(0.0)
                } else {
                    0.0
                };
                if lock.weight <= 0.0 {
                    *pinned = false;
                    if now.is_finite() {
                        lock.target = now;
                    }
                }
            } else {
                if now.is_finite() {
                    lock.target = now;
                }
                lock.weight = 0.0;
            }
        }
        &self.locks
    }
}

fn lock_bits(lock: &FootLock) -> (bool, [u32; 3], u32) {
    (
        lock.planted,
        lock.target.to_array().map(f32::to_bits),
        lock.weight.to_bits(),
    )
}

#[test]
fn motion_matching_foot_locker_is_bit_identical() {
    for (seed, settings) in [
        (0x9e37_79b9_7f4a_7c15u64, FootLockSettings::default()),
        (
            0x1234_5678_9abc_def1,
            FootLockSettings {
                release_time: 0.0,
                max_drift: 0.05,
            },
        ),
        (
            0x0f0f_f0f0_1357_2468,
            FootLockSettings {
                release_time: f32::NAN,
                max_drift: f32::INFINITY,
            },
        ),
        (
            0xdead_beef_cafe_f00d,
            FootLockSettings {
                release_time: 0.4,
                max_drift: f32::NAN,
            },
        ),
    ] {
        let mut rng = Rng(seed);
        let mut locker = FootLocker::new(settings);
        let mut frozen = FrozenLocker {
            settings,
            ..FrozenLocker::default()
        };
        let mut bare: Vec<PlantLock> = Vec::new();
        let mut feet = vec![Vec3::ZERO; 3];
        for step in 0..2000 {
            let n = if step % 211 < 7 { 2 } else { 3 };
            let planted: Vec<bool> = (0..n).map(|_| rng.unit() < 0.6).collect();
            for foot in &mut feet {
                let r = rng.unit();
                *foot +=
                    Vec3::new(rng.unit() - 0.5, 0.0, rng.unit() - 0.5) * if r < 0.05 { 4.0 } else { 0.08 };
            }
            let mut animated: Vec<Vec3> = feet[..n].to_vec();
            if step % 97 == 13 {
                animated[0] = Vec3::NAN;
            }
            let dt = match step % 173 {
                5 => 10.0,
                9 => f32::NAN,
                11 => -1.0,
                _ => 1.0 / 60.0,
            };
            if step == 1000 {
                locker.reset();
                frozen.locks.clear();
                frozen.pinned.clear();
                bare.clear();
            }
            let new = locker.update(&planted, &animated, dt).to_vec();
            let old = frozen.update(&planted, &animated, dt).to_vec();
            assert_eq!(new.len(), old.len());
            for (foot, (a, b)) in new.iter().zip(&old).enumerate() {
                assert_eq!(
                    lock_bits(a),
                    lock_bits(b),
                    "seed {seed:#x} step {step} foot {foot}"
                );
            }
            // The same through a bare PlantLock with a fixed release and 3D drift.
            let dt_s = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
            let release = if settings.release_time.is_finite() {
                settings.release_time.max(0.0)
            } else {
                0.0
            };
            let drift = if settings.max_drift.is_finite() {
                settings.max_drift.max(0.0)
            } else {
                f32::INFINITY
            };
            bare.resize(n, PlantLock::default());
            for (foot, plant) in bare.iter_mut().enumerate() {
                let now = animated[foot];
                let down = planted[foot] && now.is_finite();
                let repin = plant.drifted(plant.pin().distance(now), drift);
                plant.update(down, repin, now, dt_s, release, PlantRelease::Fixed);
                assert_eq!(plant.weight().to_bits(), new[foot].weight.to_bits());
                if plant.weight() > 0.0 {
                    assert_eq!(plant.pin().to_array(), new[foot].target.to_array());
                }
            }
        }
    }
}

/// Recorded on the lane base (`bb01ec793` + the gltf-plain-node-tracks merge, `7d741822f`) before the
/// refactor, x86_64 Linux. The scenario uses no transcendental the crate does not make portable, but
/// cross-arch identity is not claimed: rerecord only for an intended output change, with the reason.
///
/// Re-recorded at integ-v06r-det (x86_64 Linux) for ADR-8388's intended planter change (lane
/// buildout-anim-foot-component): the ground height is smoothed in world Y and a planted foot that moved to a
/// new ground sample stands on it at once. The digest before that change was `0x0215_beef_231d_84a1`; the crowd
/// and motion-matching identities below are unchanged.
///
/// Re-recorded at integ-v061b (x86_64 Linux) for the intended planter changes of anim-foot-component-fixes and
/// its -b lane (ADR-8721, ADR-8801): the stance ends at 1.5 times `lock_max_speed` at every frame rate, `pin` is
/// `None` during a release, `planted` is 0 while the effect is faded out (this walk fades out for frames 800-829),
/// and the lift-off ground is a floor until the release ends. The digest before those changes was
/// `0x7f0f_4f52_2b0e_a552`; the crowd and motion-matching identities below are unchanged.
const PLANTER_DIGEST: u64 = 0xad30_ad86_0e15_c79d;
/// Recorded on the lane base before the refactor.
const CROWD_DIGEST: u64 = 0xa68f_b2d9_a1db_7476;
/// Recorded on the lane base before the refactor.
const CROWD_LOCKED: usize = 1715;
