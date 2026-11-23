// SPDX-License-Identifier: MPL-2.0
//! Locomotion cost (animation-11, ADR-8728): the foot planter's `_into` forms allocate nothing
//! after warm-up and give the allocating forms' bits, and the IK level of detail of
//! `FootPlacement` and `GaitRig` probes on its cadence and leaves the clip pose when off.
//!
//! These tests fail to compile on the base, which has no `plan_into`, `report_into` or
//! `ik_lod`. This binary installs a counting global allocator (per thread).

#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{
    FootGround, FootLegDesc, FootPlacement, FootPlacementStatus, FootPlant, FootPlantInput, FootPlantLeg,
    FootPlantReport, FootPlantSettings, FootPlanter, GroundHit, GroundProbe, GroundProbes,
};
use regia_animation::gait::{GaitParams, GaitPatternSet, GaitPosture, MAX_LEGS, TRIPOD_LEGS, leg_array};
use regia_animation::gait_rig::{GaitLeg, GaitRig, GaitRigStatus};
use regia_animation::ik_lod::{IkLod, IkLodSettings};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimLodCamera, AnimationPlugin, Animator, JointGlobals, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator and counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

// ---------------------------------------------------------------- planter

const ANKLE: f32 = 0.128;

/// `sin` and `cos` from a fixed f64 Taylor series: the same bits on every target (the platform
/// math libraries differ in the last bit, which would move the scenario's inputs, not the planter).
fn det_sin_cos(x: f32) -> (f32, f32) {
    let x = f64::from(x);
    let (mut sin, mut cos) = (0.0_f64, 0.0_f64);
    let mut term = 1.0_f64; // x^n / n!
    for n in 0..24_u32 {
        match n % 4 {
            0 => cos += term,
            1 => sin += term,
            2 => cos -= term,
            _ => sin -= term,
        }
        term = term * x / f64::from(n + 1);
    }
    (sin as f32, cos as f32)
}

fn slope_normal(deg: f32) -> Vec3 {
    let (s, c) = det_sin_cos(deg * (std::f32::consts::PI / 180.0));
    Vec3::new(-s, c, 0.0)
}

/// A rotation about +Y without platform trigonometry.
fn yaw(angle: f32) -> Quat {
    let (s, c) = det_sin_cos(angle * 0.5);
    Quat::from_xyzw(0.0, s, 0.0, c)
}

/// Frame `k` of a varied two-leg scenario (the planter unit test `two_runs_agree_bit_for_bit`):
/// moving, turning rig, ground and slope changing under one foot, lifts, an airborne gap.
fn scenario_legs(k: usize) -> ([FootPlantLeg; 2], Vec3, Quat, bool) {
    let phase = k as f32 * 0.05;
    let leg = |side: f32, ground_y: f32, normal: Vec3| {
        let foot = Vec3::new(0.197 * side, ANKLE, 0.0);
        FootPlantLeg {
            hip: Vec3::new(0.109 * side, 0.919, 0.03),
            foot,
            length: 0.8,
            ankle_height: ANKLE,
            ground: FootGround::Contact(Vec3::new(foot.x, ground_y, foot.z)),
            normal,
            contact: None,
        }
    };
    let mut legs = [
        leg(1.0, 0.1 * (phase * 0.7).fract(), slope_normal(10.0 + phase)),
        leg(-1.0, -0.05, slope_normal(5.0)),
    ];
    legs[0].foot.y = ANKLE + 0.1 * (phase.fract() - 0.5).max(0.0);
    legs[1].foot.z = 0.2 * (phase * 0.3).fract();
    (legs, Vec3::new(phase, 0.0, -phase), yaw(phase * 0.1), k % 70 < 60)
}

fn scenario_input(
    legs: &[FootPlantLeg],
    translation: Vec3,
    rotation: Quat,
    active: bool,
) -> FootPlantInput<'_> {
    FootPlantInput {
        rig_translation: translation,
        rig_rotation: rotation,
        legs,
        active,
        dt: DT,
    }
}

fn plan_bits(plan: &FootPlant) -> Vec<u32> {
    let mut bits = vec![plan.pelvis_offset.to_bits(), plan.weight.to_bits()];
    for leg in &plan.legs {
        bits.extend(leg.target.to_array().map(f32::to_bits));
        bits.extend(leg.tilt.to_array().map(f32::to_bits));
        bits.extend(leg.toe.to_array().map(f32::to_bits));
        bits.extend([leg.planted, leg.lock, leg.heel_gap, leg.toe_gap].map(f32::to_bits));
    }
    bits
}

/// `plan_into` gives exactly the bits `plan` gives, frame after frame (moving and turning rig,
/// ground changes, lifts, an airborne fade), with an output buffer reused across frames.
#[test]
fn plan_into_matches_plan_bit_for_bit() {
    let mut allocating = FootPlanter::new(FootPlantSettings::default());
    let mut reusing = FootPlanter::new(FootPlantSettings::default());
    let mut out = FootPlant::default();
    for k in 0..400 {
        let (legs, translation, rotation, active) = scenario_legs(k);
        let input = scenario_input(&legs, translation, rotation, active);
        let expected = allocating.plan(&input);
        reusing.plan_into(&input, &mut out);
        assert_eq!(plan_bits(&out), plan_bits(&expected), "frame {k}");
    }
    assert_eq!(allocating, reusing, "the planters' states agree");
}

/// After a warm-up (leg state and output grown), `plan_into` allocates nothing, also across an
/// airborne fade-out and back.
#[test]
fn plan_into_allocates_nothing_after_warmup() {
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut out = FootPlant::default();
    for k in 0..10 {
        let (legs, translation, rotation, active) = scenario_legs(k);
        planter.plan_into(&scenario_input(&legs, translation, rotation, active), &mut out);
    }
    let mut counts = Vec::new();
    for k in 10..300 {
        let (legs, translation, rotation, active) = scenario_legs(k);
        let input = scenario_input(&legs, translation, rotation, active);
        let before = allocations();
        planter.plan_into(&input, &mut out);
        counts.push(allocations() - before);
    }
    assert!(counts.iter().all(|c| *c == 0), "plan_into allocated: {counts:?}");
}

/// `report_into` refills a report without allocating once its vectors have grown.
#[test]
fn report_into_allocates_nothing() {
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut out = FootPlant::default();
    let mut report = FootPlantReport::default();
    for k in 0..5 {
        let (legs, translation, rotation, active) = scenario_legs(k);
        planter.plan_into(&scenario_input(&legs, translation, rotation, active), &mut out);
        planter.report_into(&out, &mut report);
    }
    let mut total = 0;
    for k in 5..200 {
        let (legs, translation, rotation, active) = scenario_legs(k);
        planter.plan_into(&scenario_input(&legs, translation, rotation, active), &mut out);
        let before = allocations();
        planter.report_into(&out, &mut report);
        total += allocations() - before;
        assert_eq!(report, planter.report(&out), "frame {k}");
    }
    assert_eq!(total, 0, "report_into allocated");
}

/// FNV-1a over the bits of 200 frames of the planter scenario (`plan` outputs: pelvis, weight,
/// every leg's target, tilt, toe, planted, lock and gaps), the scenario of the planter's
/// `two_runs_agree_bit_for_bit` with inputs built without platform trigonometry.
fn plant_scenario_digest() -> u64 {
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut out = FootPlant::default();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for k in 0..200 {
        let (legs, translation, rotation, active) = scenario_legs(k);
        planter.plan_into(&scenario_input(&legs, translation, rotation, active), &mut out);
        for bits in plan_bits(&out) {
            for byte in bits.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    hash
}

/// Prints the planter scenario digest for the cross-architecture comparison (ADR-8728) and
/// checks it is stable within one run. Run with `--nocapture` on each target and compare.
#[test]
fn plant_scenario_digest_is_stable() {
    let digest = plant_scenario_digest();
    println!(
        "plant_scenario_digest arch={} scalar_math={} digest={digest:016x}",
        std::env::consts::ARCH,
        cfg!(feature = "cross-platform-determinism")
    );
    assert_eq!(digest, plant_scenario_digest());
}

/// With `cross-platform-determinism` (glam's scalar path, ADR-5176) the planter gives the same bits
/// on x86_64 and aarch64 (ADR-8728). Measured: Linux x86_64, Windows x86_64 (MSVC), macOS aarch64
/// and Android aarch64 (TEST Pixel 9 Pro). Without the feature the two architectures differ (glam's
/// SSE2 and NEON paths sum and fuse differently), so this pin only exists with the feature.
#[cfg(feature = "cross-platform-determinism")]
#[test]
fn plant_scenario_digest_is_pinned_across_architectures() {
    assert_eq!(
        format!("{:016x}", plant_scenario_digest()),
        "57beb214bc0d061f",
        "the planter's bits moved; re-pin only with a proven cause on every architecture"
    );
}

// ---------------------------------------------------------------- FootPlacement

/// A flat ground at `y` that counts its probes.
struct CountingFloor {
    y: f32,
    probes: Arc<AtomicUsize>,
}

impl GroundProbe for CountingFloor {
    fn probe(&self, from: Vec3, _down: Vec3, _max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        self.probes.fetch_add(1, Ordering::Relaxed);
        Some(GroundHit {
            point: Vec3::new(from.x, self.y, from.z),
            normal: Vec3::Y,
        })
    }
}

fn pin_schedules(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

fn walk_clip() -> (Arc<Skeleton>, AnimationClip) {
    let skeleton = demo_humanoid(&DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    })
    .expect("demo rig");
    let gait = DemoGait::standard()
        .into_iter()
        .find(|gait| gait.name == "walk")
        .expect("walk gait");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("walk clip"),
    );
    (Arc::new(skeleton), clip)
}

fn placement(lod: IkLodSettings) -> FootPlacement {
    let leg = |side: &str| FootLegDesc {
        hip: format!("thigh_{side}"),
        knee: format!("calf_{side}"),
        ankle: format!("foot_{side}"),
        toe: None,
        pole_offset: Vec3::ZERO,
    };
    let mut placement = FootPlacement::default();
    placement.legs = vec![leg("l"), leg("r")];
    placement.lod = lod;
    placement
}

/// An app with a raised floor (so planting moves the feet) and a probe counter.
fn feet_app(floor: f32) -> (App, Arc<AtomicUsize>) {
    let probes = Arc::new(AtomicUsize::new(0));
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    app.insert_resource(GroundProbes::new(CountingFloor {
        y: floor,
        probes: probes.clone(),
    }));
    (app, probes)
}

fn spawn_walker(
    app: &mut App,
    skeleton: &Arc<Skeleton>,
    clip: &AnimationClip,
    feet: Option<FootPlacement>,
) -> Entity {
    let animator = Animator::single(clip.clone(), true);
    let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
    let mut entity = app.world_mut().spawn((Transform::default(), binding, animator));
    if let Some(feet) = feet {
        entity.insert(feet);
    }
    entity.id()
}

fn globals_bits(app: &App, rig: Entity) -> Vec<u32> {
    let globals = app.world().get::<JointGlobals>(rig).expect("JointGlobals");
    bits(&globals.globals)
}

fn bits(m: &[Mat4]) -> Vec<u32> {
    m.iter()
        .flat_map(|m| m.to_cols_array())
        .map(f32::to_bits)
        .collect()
}

fn force(app: &mut App, rig: Entity, lod: Option<IkLod>) {
    app.world_mut()
        .get_mut::<FootPlacement>(rig)
        .expect("placement")
        .lod
        .force = lod;
}

/// At reduced detail the feet are probed every `reduced_interval` frames (the last hits reused
/// in between); at full detail every frame. The camera distance picks the level, `force` wins.
#[test]
fn lod_reduced_probes_every_n_frames() {
    let (skeleton, clip) = walk_clip();
    let (mut app, probes) = feet_app(0.1);
    let lod = IkLodSettings {
        full_within: 10.0,
        reduced_within: 40.0,
        reduced_interval: 4,
        force: None,
    };
    let rig = spawn_walker(&mut app, &skeleton, &clip, Some(placement(lod)));
    // The camera 5 m away: full.
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(0.0, 1.5, 5.0),
    });
    for _ in 0..30 {
        app.update();
    }
    let status = |app: &App| *app.world().get::<FootPlacementStatus>(rig).expect("status");
    assert_eq!(status(&app).lod, IkLod::Full);
    let before = probes.load(Ordering::Relaxed);
    for _ in 0..40 {
        app.update();
    }
    let full = probes.load(Ordering::Relaxed) - before;
    assert_eq!(full, 40 * 2, "full detail probes both feet every frame");

    // The camera 25 m away: reduced, every fourth frame.
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(0.0, 1.5, 25.0),
    });
    let before = probes.load(Ordering::Relaxed);
    let mut frames_with_probes = 0;
    for _ in 0..40 {
        let at = probes.load(Ordering::Relaxed);
        app.update();
        if probes.load(Ordering::Relaxed) > at {
            frames_with_probes += 1;
        }
        assert_eq!(status(&app).lod, IkLod::Reduced);
    }
    let reduced = probes.load(Ordering::Relaxed) - before;
    assert_eq!(frames_with_probes, 10, "one probing frame in four");
    assert_eq!(reduced, 10 * 2, "both feet on a probing frame");
    // The plan still runs every frame on the reused hits: the effect stays on.
    let report = app.world().get::<FootPlantReport>(rig).expect("report");
    assert!(report.weight > 0.99, "weight {}", report.weight);

    // A host-set level wins over the camera.
    force(&mut app, rig, Some(IkLod::Full));
    let before = probes.load(Ordering::Relaxed);
    for _ in 0..8 {
        app.update();
    }
    assert_eq!(probes.load(Ordering::Relaxed) - before, 8 * 2);
    assert_eq!(status(&app).lod, IkLod::Full);
}

/// Off: no probes, and once the effect has faded (at the blend-out rate) the rig's joints are
/// bit-identical to the same rig without `FootPlacement`. A rig that starts off never plans.
#[test]
fn lod_off_leaves_the_clip_pose() {
    let (skeleton, clip) = walk_clip();
    let (mut app, probes) = feet_app(0.15);
    let plain = spawn_walker(&mut app, &skeleton, &clip, None);
    let fading = spawn_walker(
        &mut app,
        &skeleton,
        &clip,
        Some(placement(IkLodSettings::default())),
    );
    let never = spawn_walker(
        &mut app,
        &skeleton,
        &clip,
        Some(placement(IkLodSettings {
            force: Some(IkLod::Off),
            ..IkLodSettings::default()
        })),
    );
    for _ in 0..40 {
        app.update();
    }
    assert_ne!(
        globals_bits(&app, fading),
        globals_bits(&app, plain),
        "at full detail the raised floor moves the feet"
    );
    assert_eq!(
        globals_bits(&app, never),
        globals_bits(&app, plain),
        "never planned"
    );
    let off_status = *app.world().get::<FootPlacementStatus>(never).expect("status");
    assert_eq!(off_status.lod, IkLod::Off);

    // Switch the planting rig off: the effect fades out on the cached hits, then the clip pose.
    force(&mut app, fading, Some(IkLod::Off));
    let before = probes.load(Ordering::Relaxed);
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(probes.load(Ordering::Relaxed), before, "off detail never probes");
    assert_eq!(
        globals_bits(&app, fading),
        globals_bits(&app, plain),
        "the clip pose"
    );
    let report = app.world().get::<FootPlantReport>(fading).expect("report");
    assert_eq!(report.weight, 0.0);

    // Back to full: probes and plants again.
    force(&mut app, fading, None);
    for _ in 0..30 {
        app.update();
    }
    assert!(probes.load(Ordering::Relaxed) > before);
    assert_ne!(globals_bits(&app, fading), globals_bits(&app, plain));
}

/// A planting rig allocates exactly as often per settled frame as the same rig without
/// `FootPlacement` (the planter, the report and the apply reuse their buffers), at full and at
/// reduced detail.
#[test]
fn foot_placement_adds_no_allocations() {
    fn frame_counts(feet: Option<IkLod>) -> usize {
        let (skeleton, clip) = walk_clip();
        let (mut app, _probes) = feet_app(0.1);
        pin_schedules(&mut app);
        let lod = IkLodSettings {
            force: feet,
            ..IkLodSettings::default()
        };
        let rig = spawn_walker(&mut app, &skeleton, &clip, feet.map(|_| placement(lod)));
        for _ in 0..60 {
            app.update();
        }
        pin_schedules(&mut app);
        for _ in 0..120 {
            app.update();
        }
        let _ = rig;
        (0..60)
            .map(|_| {
                let before = allocations();
                app.update();
                allocations() - before
            })
            .sum()
    }
    let plain = frame_counts(None);
    let full = frame_counts(Some(IkLod::Full));
    let reduced = frame_counts(Some(IkLod::Reduced));
    println!("allocations over 60 settled frames: plain {plain}, full {full}, reduced {reduced}");
    assert_eq!(full, plain, "a planting rig at full detail allocated");
    assert_eq!(reduced, plain, "a planting rig at reduced detail allocated");
}

/// Nine resolved legs miss the eight-slot stack in `FootPlacement::advance` and build
/// `owned`, `owned_soles` and `mids` every frame. The module contract is that a settled
/// frame allocates nothing. The two-leg test above stays on the stack and does not see this.
#[test]
fn review_proof_nine_leg_foot_placement_settled_frame_allocates_nothing() {
    const LEGS: usize = 9;
    let skeleton = many_leg_skeleton(LEGS);
    let clip = AnimationClip::empty("still", 1.0);
    fn frame_counts(skeleton: &Arc<Skeleton>, clip: &AnimationClip, plant: bool) -> usize {
        let (mut app, _probes) = feet_app(0.0);
        pin_schedules(&mut app);
        let feet = plant.then(|| many_placement(LEGS));
        let rig = spawn_walker(&mut app, skeleton, clip, feet);
        for _ in 0..60 {
            app.update();
        }
        if plant {
            let status = app.world().get::<FootPlacementStatus>(rig).expect("status");
            assert!(status.legs_resolved, "nine named legs resolve");
            assert!(status.probe_installed, "the floor probe is installed");
            let report = app.world().get::<FootPlantReport>(rig).expect("report");
            assert!(
                report.weight > 0.5,
                "the planter is in steady state, weight {}",
                report.weight
            );
        }
        pin_schedules(&mut app);
        for _ in 0..30 {
            app.update();
        }
        (0..60)
            .map(|_| {
                let before = allocations();
                app.update();
                allocations() - before
            })
            .sum()
    }
    let plain = frame_counts(&skeleton, &clip, false);
    let planting = frame_counts(&skeleton, &clip, true);
    println!("nine-leg allocations over 60 settled frames: plain {plain}, planting {planting}");
    assert_eq!(
        planting, plain,
        "a nine-leg planting rig allocated on a settled frame"
    );
}

fn many_leg_skeleton(legs: usize) -> Arc<Skeleton> {
    let mut names = vec!["root".to_string()];
    let mut parents = vec![None];
    let mut binds = vec![LocalBind {
        translation: Vec3::new(0.0, 1.0, 0.0),
        ..LocalBind::default()
    }];
    for leg in 0..legs {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = (leg / 2) as f32 * 0.2;
        let mut parent = 0_u32;
        let joints = [
            (format!("hip{leg}"), Vec3::new(side * 0.15, 0.0, z)),
            (format!("knee{leg}"), Vec3::new(0.0, -0.45, 0.0)),
            (format!("foot{leg}"), Vec3::new(0.0, -0.45, 0.0)),
        ];
        for (name, translation) in joints {
            names.push(name);
            parents.push(Some(parent));
            binds.push(LocalBind {
                translation,
                ..LocalBind::default()
            });
            parent = (names.len() - 1) as u32;
        }
    }
    Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("many legs"))
}

fn many_placement(legs: usize) -> FootPlacement {
    let mut placement = FootPlacement::default();
    placement.legs = (0..legs)
        .map(|leg| FootLegDesc {
            hip: format!("hip{leg}"),
            knee: format!("knee{leg}"),
            ankle: format!("foot{leg}"),
            toe: None,
            pole_offset: Vec3::ZERO,
        })
        .collect();
    placement
}

// ---------------------------------------------------------------- GaitRig

const BODY_HEIGHT: f32 = 0.5;
const PAIR_Z: [f32; 3] = [-0.5, 0.0, 0.5];

/// A two-bone hexapod: `root` at the floor, `body` 0.5 m up, six legs with tips on the floor
/// (the fixture of `tests/gait_rig.rs`).
fn hexapod() -> Arc<Skeleton> {
    let mut names = vec!["root".to_string(), "body".to_string()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        LocalBind {
            translation: Vec3::new(0.0, BODY_HEIGHT, 0.0),
            ..LocalBind::default()
        },
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = PAIR_Z[leg / 2];
        let mut parent = 1_u32;
        let joints = [
            (format!("hip{leg}"), Vec3::new(side * 0.25, 0.0, z)),
            (format!("knee{leg}"), Vec3::new(side * 0.2, 0.15, 0.0)),
            (format!("foot{leg}"), Vec3::new(side * 0.15, -0.65, 0.0)),
        ];
        for (name, translation) in joints {
            names.push(name);
            parents.push(Some(parent));
            binds.push(LocalBind {
                translation,
                ..LocalBind::default()
            });
            parent = (names.len() - 1) as u32;
        }
    }
    Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("hexapod"))
}

fn gait_params() -> GaitParams {
    GaitParams {
        forward_speed: 0.6,
        reverse_speed: 0.3,
        strafe_speed: 0.3,
        acceleration: 2.0,
        step_length: 0.3,
        clearance: BODY_HEIGHT,
        swing_lift: 0.1,
        max_slope: 1.5,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array([[0.0; 2]; TRIPOD_LEGS], [0.0; 2]),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.1,
        turn_step_angle: 0.35,
        ground_band: 0.6,
        foothold_candidates: 0,
        foothold_spacing: 0.2,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.2,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    }
}

fn gait_rig(lod: IkLodSettings) -> GaitRig {
    let legs = (0..TRIPOD_LEGS)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect();
    let mut rig = GaitRig::new(gait_params(), legs).with_body("body");
    rig.lod = lod;
    rig
}

fn spawn_hexapod(app: &mut App, skeleton: &Arc<Skeleton>, rig: Option<GaitRig>) -> Entity {
    let animator = Animator::single(AnimationClip::empty("still", 1.0), true);
    let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
    let mut entity = app.world_mut().spawn((Transform::default(), binding, animator));
    if let Some(rig) = rig {
        entity.insert(rig);
    }
    entity.id()
}

fn drive(app: &mut App, rigs: &[Entity], frame: usize) {
    for &rig in rigs {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("transform")
            .translation
            .z = -0.4 * DT * frame as f32;
    }
}

/// A walking hexapod at reduced detail probes only on every fourth frame, keeps walking on the
/// cached hits (no refused step), and its status says `reduced`.
#[test]
fn gait_rig_lod_reduced_probes_every_n_frames() {
    let skeleton = hexapod();
    let (mut app, probes) = feet_app(0.0);
    let rig = spawn_hexapod(
        &mut app,
        &skeleton,
        Some(gait_rig(IkLodSettings {
            force: Some(IkLod::Reduced),
            reduced_interval: 4,
            ..IkLodSettings::default()
        })),
    );
    let mut probing_frames = Vec::new();
    for frame in 0..240 {
        drive(&mut app, &[rig], frame);
        let at = probes.load(Ordering::Relaxed);
        app.update();
        if probes.load(Ordering::Relaxed) > at {
            probing_frames.push(frame);
        }
    }
    let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
    assert_eq!(status.lod, IkLod::Reduced);
    assert_eq!(status.step_errors, 0, "the cached hits carry the walk");
    assert!(
        probing_frames.len() > 2,
        "the rig probes at all: {probing_frames:?}"
    );
    // The first frame probes because nothing is cached yet; every later probing frame is on the
    // rig's cadence: one residue modulo four (the entity's stagger).
    let later = &probing_frames[1..];
    let residue = later[0] % 4;
    assert!(
        later.iter().all(|f| f % 4 == residue),
        "probing frames {probing_frames:?}"
    );
    assert!(probing_frames.len() <= 240 / 4 + 1, "{probing_frames:?}");
}

/// A driven hexapod that is off is neither stepped nor posed: its joints are bit-identical to
/// the same rig without `GaitRig`, and it probes nothing. Back at full detail it walks again.
#[test]
fn gait_rig_lod_off_leaves_the_clip_pose() {
    let skeleton = hexapod();
    let (mut app, probes) = feet_app(0.0);
    let plain = spawn_hexapod(&mut app, &skeleton, None);
    let walker = spawn_hexapod(
        &mut app,
        &skeleton,
        Some(gait_rig(IkLodSettings {
            force: Some(IkLod::Off),
            ..IkLodSettings::default()
        })),
    );
    for frame in 0..120 {
        drive(&mut app, &[plain, walker], frame);
        app.update();
        assert_eq!(
            globals_bits(&app, walker),
            globals_bits(&app, plain),
            "frame {frame}"
        );
    }
    assert_eq!(probes.load(Ordering::Relaxed), 0, "off never probes");
    let status = *app.world().get::<GaitRigStatus>(walker).expect("status");
    assert_eq!(status.lod, IkLod::Off);
    app.world_mut().get_mut::<GaitRig>(walker).expect("rig").lod.force = None;
    for frame in 120..240 {
        drive(&mut app, &[plain, walker], frame);
        app.update();
    }
    let status = *app.world().get::<GaitRigStatus>(walker).expect("status");
    assert_eq!(status.lod, IkLod::Full);
    assert_eq!(status.step_errors, 0, "back from off it starts from rest");
    assert!(probes.load(Ordering::Relaxed) > 0);
    assert_ne!(globals_bits(&app, walker), globals_bits(&app, plain));
}
