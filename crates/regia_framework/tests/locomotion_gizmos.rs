// SPDX-License-Identifier: MPL-2.0
//! The `locomotion` gizmo overlay (ADR-8729): off by default and then it draws nothing; on, each
//! scripted frame draws the primitive counts its walker, foot planter and leg IK call for. The
//! counts come from the extracted gizmo frame (segments the renderer would draw), no image.
//! These tests fail to compile on the base, which has no overlay.
//!
//! `locomotion_overlay_matches_the_lavapipe_golden` draws the overlay with every toggle on
//! through the renderer's gizmo pass and compares it with `docs/evidence/gizmos/locomotion_all.png`.
//!
//! This binary installs a counting global allocator for the allocation test.

#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::chain_ik::JointLimit;
use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{
    FootLegDesc, FootPlacement, FootPlantReport, GroundHit, GroundProbe, GroundProbes,
};
use regia_animation::gait::{
    GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, TRIPOD_LEGS, leg_array, swing_arc_point,
};
use regia_animation::gait_rig::{GaitControls, GaitLeg, GaitRig, GaitRigMode};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
use regia_framework::locomotion_gizmos::{
    ARC_SEGMENTS, CONE_RAYS, HINGE_SEGMENTS, LocomotionGizmoColors, LocomotionGizmoStats, LocomotionGizmos,
    LocomotionGizmosPlugin, MAX_LOCOMOTION_SEGMENTS, REACH_SEGMENTS, RING_SEGMENTS, draw_gait,
};
use regia_render::gizmos::{GizmoBuffer, GizmoConfigGroup, GizmoConfigStore, GizmoPlugin};
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

/// A plane through the origin rising `tan` per meter along `uphill`.
#[derive(Clone, Copy)]
struct Slope {
    uphill: Vec3,
    tan: f32,
}

impl Slope {
    fn flat() -> Self {
        Self {
            uphill: Vec3::NEG_Z,
            tan: 0.0,
        }
    }

    fn degrees(uphill: Vec3, degrees: f32) -> Self {
        Self {
            uphill: uphill.normalize(),
            tan: degrees.to_radians().tan(),
        }
    }

    fn normal(&self) -> Vec3 {
        (Vec3::Y - self.uphill * self.tan).normalize()
    }
}

impl GroundProbe for Slope {
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let down = down.normalize_or_zero();
        let normal = self.normal();
        let denom = down.dot(normal);
        if denom.abs() < 1.0e-8 {
            return None;
        }
        let t = -from.dot(normal) / denom;
        if !t.is_finite() || t < 0.0 || t > max_distance {
            return None;
        }
        Some(GroundHit {
            point: from + down * t,
            normal,
        })
    }
}

/// Leg segments from the hip (unit side; X sign applied per side).
fn segments(four: bool) -> &'static [[f32; 3]] {
    if four {
        &[
            [0.15, 0.1, 0.0],
            [0.15, 0.05, 0.0],
            [0.1, -0.3, 0.0],
            [0.0, -0.35, 0.0],
        ]
    } else {
        &[[0.2, 0.15, 0.0], [0.15, -0.65, 0.0]]
    }
}

/// A hexapod: `root` at the floor, `body` 0.5 m up, six legs with tips on the floor, −Z forward.
fn hexapod(four: bool) -> Arc<Skeleton> {
    let mut names = vec!["root".to_string(), "body".to_string()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        LocalBind {
            translation: Vec3::new(0.0, 0.5, 0.0),
            ..LocalBind::default()
        },
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = [-0.5, 0.0, 0.5][leg / 2];
        let count = segments(four).len() + 1;
        let mut parent = 1_u32;
        for j in 0..count {
            let name = match j {
                0 => format!("hip{leg}"),
                j if j + 1 == count => format!("foot{leg}"),
                j => format!("seg{leg}_{j}"),
            };
            let translation = if j == 0 {
                Vec3::new(side * 0.25, 0.0, z)
            } else {
                let [x, y, z] = segments(four)[j - 1];
                Vec3::new(side * x, y, z)
            };
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

fn params() -> GaitParams {
    GaitParams {
        forward_speed: 0.6,
        reverse_speed: 0.3,
        strafe_speed: 0.3,
        acceleration: 2.0,
        step_length: 0.3,
        clearance: 0.5,
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

/// Limits of a five-joint leg, root first: ball, hinge, hinge, ball, (tip unused).
fn leg_limits() -> Vec<JointLimit> {
    let cone = JointLimit::Cone {
        max_swing: 1.2,
        min_twist: -0.4,
        max_twist: 0.4,
    };
    let hinge = JointLimit::Hinge {
        axis: Vec3::Z,
        min: -1.5,
        max: 1.5,
    };
    vec![cone, hinge, hinge, cone, JointLimit::Free]
}

/// A test app: animation, the gizmo plugin with the runtime extraction it hands batches to, and
/// the overlay. Every schedule runs on the test thread (the allocation counter is per thread).
fn app(probe: Slope) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    app.insert_resource(GroundProbes::new(probe));
    app.init_resource::<regia_render::ExtractedFrame>()
        .init_resource::<regia_render::RuntimeRenderer>()
        .add_systems(Render, regia_render::runtime::extract_frame)
        .add_systems(Render, regia_render::runtime::render_texture_views)
        .add_plugins(GizmoPlugin)
        .add_plugins(LocomotionGizmosPlugin);
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

fn spawn_hexapod(app: &mut App, four: bool) -> Entity {
    let skeleton = hexapod(four);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let legs = (0..TRIPOD_LEGS)
        .map(|i| {
            let mut leg = GaitLeg::new(format!("hip{i}"), format!("foot{i}"));
            if four {
                leg.limits = leg_limits();
            }
            leg
        })
        .collect();
    let rig = GaitRig::new(params(), legs)
        .with_body("body")
        .with_mode(GaitRigMode::Integrating);
    app.world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(AnimationClip::empty("still", 1.0), true),
            rig,
            GaitControls {
                forward: 1.0,
                ..GaitControls::default()
            },
        ))
        .id()
}

fn spawn_biped(app: &mut App) -> Entity {
    let rig = DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    };
    let skeleton = Arc::new(demo_humanoid(&rig).expect("demo rig"));
    let gait = DemoGait::standard()
        .into_iter()
        .find(|gait| gait.name == "walk")
        .expect("walk gait");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("walk clip"),
    );
    let mut placement = FootPlacement::default();
    placement.legs = ["l", "r"]
        .into_iter()
        .map(|side| FootLegDesc {
            hip: format!("thigh_{side}"),
            knee: format!("calf_{side}"),
            ankle: format!("foot_{side}"),
            toe: Some(format!("ball_{side}")),
            pole_offset: Vec3::ZERO,
        })
        .collect();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    app.world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(clip, true),
            placement,
        ))
        .id()
}

fn set_enabled(app: &mut App, on: bool) {
    assert!(
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled(LocomotionGizmos::NAME, on)
    );
}

fn set_toggles(app: &mut App, gait: bool, foot_placement: bool, ik: bool) {
    let mut store = app.world_mut().resource_mut::<GizmoConfigStore>();
    let (_, toggles) = store.config_mut::<LocomotionGizmos>().expect("registered");
    *toggles = LocomotionGizmos {
        gait,
        foot_placement,
        ik,
    };
}

fn stats(app: &App) -> LocomotionGizmoStats {
    *app.world().resource::<LocomotionGizmoStats>()
}

/// Segments the renderer received this frame (every enabled group; only the overlay draws).
fn extracted_segments(app: &App) -> usize {
    app.world()
        .resource::<regia_render::ExtractedFrame>()
        .gizmos
        .segment_count()
}

/// The overlay's documented primitives, summed from its per-frame counts: `(lines, strips,
/// strip vertices)`. One walker per app, so the support polygon is one line (two feet) or one
/// closed strip (three or more).
fn expected_primitives(s: &LocomotionGizmoStats) -> (u32, u32, u32) {
    let ring = (RING_SEGMENTS + 1) as u32;
    let support_line = u32::from(s.support_edges == 1);
    let support_strip = u32::from(s.support_edges >= 3);
    let lines = 4 * s.hips
        + 2 * s.refused_markers
        + 3 * s.attitude_frames
        + support_line
        + 3 * s.joints
        + 4 * s.targets
        + 4 * s.poles
        + CONE_RAYS as u32 * s.cone_limits
        + 2 * s.hinge_limits
        + s.probes
        + 4 * s.probe_hits
        + s.pins
        + 4 * s.pelvis_markers;
    let strips = s.planted_feet
        + s.swinging_feet
        + s.swing_arcs
        + support_strip
        + s.margin_rings
        + s.forced_markers
        + s.chains
        + 3 * s.reach_spheres
        + s.cone_limits
        + s.hinge_limits
        + s.pins
        + s.soles
        + s.slides;
    let vertices = ring
        * (s.planted_feet + s.swinging_feet + s.margin_rings + s.pins + s.soles + s.slides + s.cone_limits)
        + (ARC_SEGMENTS + 1) as u32 * s.swing_arcs
        + support_strip * (s.support_edges + 1)
        + 5 * s.forced_markers
        + s.joints
        + 3 * (REACH_SEGMENTS + 1) as u32 * s.reach_spheres
        + (HINGE_SEGMENTS + 1) as u32 * s.hinge_limits;
    (lines, strips, vertices)
}

/// The frame's stats agree with what it drew: the counts the system read back from its buffer
/// match the primitive formula, and the renderer got exactly that many segments.
fn assert_frame_matches(app: &App) {
    let s = stats(app);
    let (lines, strips, vertices) = expected_primitives(&s);
    assert_eq!(
        (s.lines, s.strips, s.strip_vertices),
        (lines, strips, vertices),
        "{s:?}"
    );
    let segments = (lines + vertices - strips) as usize;
    assert_eq!(extracted_segments(app), segments, "{s:?}");
}

#[test]
fn locomotion_gizmos_off_draws_nothing() {
    for scene in ["hexapod", "fabrik hexapod", "biped"] {
        let mut app = app(Slope::flat());
        match scene {
            "hexapod" => spawn_hexapod(&mut app, false),
            "fabrik hexapod" => spawn_hexapod(&mut app, true),
            _ => spawn_biped(&mut app),
        };
        // Registered, off, every sub-toggle on.
        let store = app.world().resource::<GizmoConfigStore>();
        let (config, toggles) = store.config::<LocomotionGizmos>().expect("registered");
        assert!(!config.enabled, "{scene}: the overlay starts off");
        assert_eq!(*toggles, LocomotionGizmos::default());
        for frame in 0..90 {
            app.update();
            assert_eq!(extracted_segments(&app), 0, "{scene}: frame {frame} drew");
        }
        // The draw system never ran.
        assert_eq!(stats(&app), LocomotionGizmoStats::default(), "{scene}");
        // The scene did walk / plant meanwhile: there was something to draw.
        let mut walkers = app.world_mut().query::<&GaitState>();
        let walking = walkers.iter(app.world()).count();
        let mut planters = app.world_mut().query::<&FootPlantReport>();
        let planting = planters.iter(app.world()).count();
        assert_eq!(walking + planting, 1, "{scene}");
    }
}

#[test]
fn on_draws_the_expected_primitive_counts_for_a_hexapod() {
    let mut app = app(Slope::degrees(Vec3::NEG_Z, 8.0));
    let walker = spawn_hexapod(&mut app, false);
    set_enabled(&mut app, true);
    let (mut arcs_seen, mut tripod_frames, mut stance_frames) = (0, 0, 0);
    for frame in 0..320 {
        if frame == 240 {
            // Stop: the feet settle and all six plant.
            app.world_mut()
                .get_mut::<GaitControls>(walker)
                .expect("controls")
                .forward = 0.0;
        }
        app.update();
        let s = stats(&app);
        assert_eq!(s.frames, frame as u64 + 1);
        assert_frame_matches(&app);
        if frame < 2 {
            // `GaitState` and the joint globals are inserted by the first frames' commands.
            continue;
        }
        let state = *app.world().get::<GaitState>(walker).expect("GaitState");
        let planted = state.feet().iter().filter(|f| f.planted).count() as u32;
        assert_eq!(s.walkers, 1);
        assert_eq!(
            (s.planted_feet, s.swinging_feet),
            (planted, 6 - planted),
            "frame {frame}"
        );
        assert_eq!((s.hips, s.attitude_frames, s.margin_rings), (6, 1, 1));
        assert_eq!(
            (s.refused_markers, s.forced_markers),
            (0, 0),
            "flat-ish ground, no reach limit"
        );
        // A tripod: three planted feet make a triangle; six make a hexagon or, when the middle
        // feet sit on the long sides, a quadrilateral.
        match planted {
            3 => {
                tripod_frames += 1;
                assert_eq!(s.support_edges, 3, "frame {frame}");
            }
            6 => {
                // Each side's three feet are about collinear: the middle ones may or may not be
                // hull corners (by rounding), but the polygon has four to six edges.
                stance_frames += 1;
                assert!(
                    (4..=6).contains(&s.support_edges),
                    "frame {frame}: {}",
                    s.support_edges
                );
            }
            _ => {}
        }
        // Every swing far enough along to fix its landing has one arc, and the arc passes
        // through the foot: the overlay samples the stepper's own formula.
        let mut arcs = 0;
        for leg in 0..6 {
            let (Some(swing), Some(landing)) = (state.swing(leg), state.swing_landing(&params(), leg)) else {
                continue;
            };
            arcs += 1;
            let on_arc = Vec3::from_array(swing_arc_point(
                &params(),
                swing.lift_off,
                landing,
                swing.progress,
            ));
            let foot = Vec3::from_array(state.feet[leg].position);
            assert!(
                on_arc.distance(foot) < 1.0e-4,
                "frame {frame} leg {leg}: {on_arc} vs {foot}"
            );
        }
        assert_eq!(s.swing_arcs, arcs, "frame {frame}");
        arcs_seen += arcs;
        // IK: six two-bone legs, no limits, no poles.
        assert_eq!(
            (s.chains, s.joints, s.bones, s.targets, s.poles, s.reach_spheres),
            (6, 18, 12, 6, 0, 6),
            "frame {frame}"
        );
        assert_eq!((s.cone_limits, s.hinge_limits), (0, 0));
        // No foot planter in this scene.
        assert_eq!((s.planters, s.probes, s.pins), (0, 0, 0));
    }
    assert!(arcs_seen > 100, "the walker swung its feet: {arcs_seen} arcs");
    assert!(tripod_frames > 60, "{tripod_frames} tripod frames");
    assert!(stance_frames > 0, "{stance_frames} stance frames");
}

#[test]
fn on_draws_joint_limits_for_fabrik_legs() {
    let mut app = app(Slope::flat());
    spawn_hexapod(&mut app, true);
    set_enabled(&mut app, true);
    for frame in 0..60 {
        app.update();
        assert_frame_matches(&app);
        if frame < 2 {
            continue;
        }
        let s = stats(&app);
        // Five joints per leg; per leg two ball joints and two hinges (the tip's entry unused).
        assert_eq!((s.chains, s.joints, s.bones, s.targets), (6, 30, 24, 6));
        assert_eq!((s.cone_limits, s.hinge_limits), (12, 12));
        assert_eq!(s.reach_spheres, 6);
    }
}

#[test]
fn on_draws_the_expected_primitive_counts_for_a_biped() {
    let mut app = app(Slope::degrees(Vec3::NEG_Z, 10.0));
    let biped = spawn_biped(&mut app);
    set_enabled(&mut app, true);
    let mut most_pins = 0;
    for frame in 0..180 {
        app.update();
        assert_frame_matches(&app);
        let s = stats(&app);
        if frame < 2 {
            // The report and the joint globals appear after the first applied frame.
            continue;
        }
        let report = app.world().get::<FootPlantReport>(biped).expect("report").clone();
        assert_eq!(s.walkers, 0);
        assert_eq!(s.planters, 1);
        // Both legs probe the slope and hit it; each hit has a normal and a tilted sole.
        assert_eq!((s.probes, s.probe_hits, s.soles), (2, 2, 2), "frame {frame}");
        assert_eq!(s.pelvis_markers, 1);
        let slides = report.slide.iter().filter(|s| **s > 1.0e-4).count() as u32;
        assert_eq!(s.slides, slides, "frame {frame}: {report:?}");
        assert!(s.pins <= 2);
        most_pins = most_pins.max(s.pins);
        // IK: two two-bone legs with their plan targets and the knee pole.
        assert_eq!(
            (s.chains, s.joints, s.bones, s.targets, s.poles, s.reach_spheres),
            (2, 6, 4, 2, 2, 2),
            "frame {frame}"
        );
    }
    assert!(most_pins >= 1, "a planted foot was pinned");
}

#[test]
fn sub_toggles_pick_what_is_drawn() {
    let mut app = app(Slope::flat());
    spawn_hexapod(&mut app, false);
    spawn_biped(&mut app);
    set_enabled(&mut app, true);
    for _ in 0..10 {
        app.update();
    }
    let all = stats(&app);
    assert!(
        all.walkers == 1 && all.planters == 1 && all.chains == 8,
        "{all:?}"
    );

    set_toggles(&mut app, true, false, false);
    app.update();
    let s = stats(&app);
    assert_eq!(
        (s.walkers, s.planters, s.chains, s.probes),
        (1, 0, 0, 0),
        "gait only"
    );

    set_toggles(&mut app, false, true, false);
    app.update();
    let s = stats(&app);
    assert_eq!(
        (s.walkers, s.planters, s.chains, s.probes),
        (0, 1, 0, 2),
        "foot placement only"
    );

    set_toggles(&mut app, false, false, true);
    app.update();
    let s = stats(&app);
    assert_eq!(
        (s.walkers, s.planters, s.chains, s.probes),
        (0, 0, 8, 0),
        "ik only"
    );

    set_toggles(&mut app, false, false, false);
    app.update();
    assert_eq!(extracted_segments(&app), 0, "every sub-toggle off");
    let frames = stats(&app).frames;
    set_enabled(&mut app, false);
    set_toggles(&mut app, true, true, true);
    app.update();
    assert_eq!(stats(&app).frames, frames, "off: the system does not run");
    assert_eq!(extracted_segments(&app), 0);
}

#[test]
fn identical_runs_draw_identical_frames() {
    let run = || {
        let mut app = app(Slope::degrees(Vec3::new(1.0, 0.0, -1.0), 6.0));
        spawn_hexapod(&mut app, false);
        spawn_biped(&mut app);
        set_enabled(&mut app, true);
        let mut frames = Vec::new();
        for _ in 0..90 {
            app.update();
            let gizmos = app
                .world()
                .resource::<regia_render::ExtractedFrame>()
                .gizmos
                .clone();
            let bits: Vec<u32> = gizmos
                .batches
                .iter()
                .flat_map(|batch| batch.segments.iter())
                .flat_map(|segment| format!("{segment:?}").into_bytes())
                .map(u32::from)
                .collect();
            frames.push(bits);
        }
        frames
    };
    assert!(run() == run(), "two identical runs drew different gizmos");
}

#[test]
fn colors_come_from_the_theme_tokens() {
    let colors = LocomotionGizmoColors::default();
    let theme = regia_ui::theme_resolve::builtin_theme_set()
        .resolve(regia_ui::theme_resolve::DEFAULT_THEME, None)
        .expect("default theme");
    let token = |key: &str| regia_core::math::Color::from(theme.color(key).expect(key).0);
    assert_eq!(colors.planted, token("color.success"));
    assert_eq!(colors.swinging, token("color.warning"));
    assert_eq!(colors.refused, token("color.danger"));
    assert_ne!(colors.planted, colors.swinging);
    // Every role names a token the default theme defines.
    for (role, key) in LocomotionGizmoColors::TOKENS {
        assert!(theme.color(key).is_some(), "{role}: {key}");
    }
    // A host palette replaces them.
    let custom = LocomotionGizmoColors::from_tokens(|key| {
        (key == "color.success").then_some(regia_core::math::Color::rgb(0.0, 0.0, 1.0))
    });
    assert_eq!(custom.planted, regia_core::math::Color::rgb(0.0, 0.0, 1.0));
}

#[test]
fn debug_gizmos_plugin_registers_the_overlay() {
    let mut app = App::new();
    app.add_plugins(GizmoPlugin)
        .add_plugins(regia_framework::debug_gizmos::DebugGizmosPlugin);
    let store = app.world().resource::<GizmoConfigStore>();
    assert!(store.config_by_name("locomotion").is_some_and(|c| !c.enabled));
    assert!(app.world().contains_resource::<LocomotionGizmoColors>());
}

#[test]
fn locomotion_gizmos_allocate_nothing_after_warmup() {
    // Two identical scenes, one with the overlay on. The renderer's own extraction allocates for
    // every non-empty batch, so the count covers `PostUpdate` (where the overlay draws) run on
    // its own after each full frame: after warm-up the drawing scene allocates exactly as often
    // as the other one.
    let build = |on: bool| {
        let mut app = app(Slope::degrees(Vec3::NEG_Z, 5.0));
        spawn_hexapod(&mut app, false);
        spawn_hexapod(&mut app, true);
        spawn_biped(&mut app);
        set_enabled(&mut app, on);
        app
    };
    let mut off = build(false);
    let mut on = build(true);
    let mut measure = |frames: usize| {
        let (mut counted_off, mut counted_on) = (0, 0);
        for _ in 0..frames {
            off.update();
            on.update();
            let before = allocations();
            off.world_mut().run_schedule(PostUpdate);
            counted_off += allocations() - before;
            let before = allocations();
            on.world_mut().run_schedule(PostUpdate);
            counted_on += allocations() - before;
        }
        (counted_off, counted_on, stats(&on))
    };
    measure(120);
    let (counted_off, counted_on, drawn) = measure(60);
    assert!(
        drawn.chains == 14 && drawn.walkers == 2 && drawn.planters == 1,
        "{drawn:?}"
    );
    assert_eq!(
        counted_on, counted_off,
        "drawn frames allocated {counted_on} times vs {counted_off} undrawn over 60 frames"
    );
}

/// Timing evidence, not a test: mean wall time of a full frame with the overlay on versus off, for
/// eight walking hexapods (four with FABRIK legs and limits) and two foot-planting bipeds. Prints
/// and asserts nothing; run with `--ignored --nocapture` in a release build.
#[test]
#[ignore = "timing evidence: prints, asserts nothing"]
fn locomotion_gizmos_frame_cost_evidence() {
    let build = |on: bool| {
        let mut app = app(Slope::degrees(Vec3::NEG_Z, 5.0));
        for i in 0..8 {
            let walker = spawn_hexapod(&mut app, i % 2 == 1);
            app.world_mut()
                .get_mut::<Transform>(walker)
                .expect("transform")
                .translation
                .x = 3.0 * i as f32;
        }
        for i in 0..2 {
            let biped = spawn_biped(&mut app);
            app.world_mut()
                .get_mut::<Transform>(biped)
                .expect("transform")
                .translation
                .x = -3.0 * (i + 1) as f32;
        }
        set_enabled(&mut app, on);
        app
    };
    let mut off = build(false);
    let mut on = build(true);
    for _ in 0..60 {
        off.update();
        on.update();
    }
    let frames = 600;
    let (mut t_off, mut t_on) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for _ in 0..frames {
        let start = std::time::Instant::now();
        off.update();
        t_off += start.elapsed();
        let start = std::time::Instant::now();
        on.update();
        t_on += start.elapsed();
    }
    let s = stats(&on);
    let per = |t: std::time::Duration| t.as_secs_f64() * 1.0e6 / f64::from(frames);
    println!(
        "locomotion gizmos: frame off {:.1} us, on {:.1} us, overlay + its extraction {:.1} us per frame \
         ({} walkers, {} planters, {} chains, {} segments)",
        per(t_off),
        per(t_on),
        per(t_on) - per(t_off),
        s.walkers,
        s.planters,
        s.chains,
        extracted_segments(&on)
    );
}

/// The overlay's own segment count bound: line-list pairs plus every line-strip entry.
fn buffer_bound(buffer: &GizmoBuffer<LocomotionGizmos>) -> usize {
    buffer.list_positions.len() / 2 + buffer.strip_positions.len()
}

/// Cost-gizmos review finding 4: 2,000 walkers drawn into one frame stop at the segment budget
/// (the paths overlay's 20,000) instead of growing the frame without bound; the walkers past
/// it are counted as skipped. The frame passes the budget by at most one walker's draw.
#[test]
fn locomotion_draw_stops_at_the_segment_cap() {
    let mut gait = params();
    gait.hip_offsets = leg_array(
        [
            [0.3, 0.3],
            [-0.3, 0.3],
            [0.35, 0.0],
            [-0.35, 0.0],
            [0.3, -0.3],
            [-0.3, -0.3],
        ],
        [0.0; 2],
    );
    let state = GaitState::at_rest(&gait, [0.0, 0.5, 0.0], 0.0, |_: f32, _: f32| 0.0).expect("rest");
    let colors = LocomotionGizmoColors::default();
    let mut one = GizmoBuffer::<LocomotionGizmos>::default();
    let mut stats = LocomotionGizmoStats::default();
    draw_gait(&mut one, &gait, &state, None, false, &colors, &mut stats);
    let per_walker = buffer_bound(&one);
    assert!(per_walker > 0, "one walker draws something");

    let mut buffer = GizmoBuffer::<LocomotionGizmos>::default();
    let mut stats = LocomotionGizmoStats::default();
    for _ in 0..2_000 {
        draw_gait(&mut buffer, &gait, &state, None, false, &colors, &mut stats);
    }
    let lines = buffer.list_positions.len() / 2;
    let bound = buffer_bound(&buffer);
    println!(
        "2000 walkers: {} drawn, {} skipped, bound {bound} (one walker {per_walker})",
        stats.walkers, stats.skipped
    );
    assert!(lines <= 20_000, "drew {lines} line segments with no cap");
    assert!(
        bound < MAX_LOCOMOTION_SEGMENTS + per_walker,
        "frame bound {bound} passed the budget by more than one walker ({per_walker})"
    );
    assert!(stats.skipped > 0, "nothing was skipped");
    assert_eq!(stats.walkers + stats.skipped, 2_000);
}

// ---------------------------------------------------------------------------------------------
// The overlay through the renderer's gizmo pass (lavapipe golden, v07-anim-owed-evidence)

const GOLDEN_W: u32 = 640;
const GOLDEN_H: u32 = 360;
/// The bar of the engine's other gizmo golden (`regia_render/tests/gizmos.rs`).
const GOLDEN_SSIM_MIN: f64 = 0.98;

fn golden_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/gizmos/locomotion_all.png")
}

fn golden_ground() -> Vec<regia_render::LabVertex> {
    let half = 40.0;
    [[-half, -half], [half, -half], [half, half], [-half, half]]
        .into_iter()
        .map(|[x, z]| regia_render::LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

/// Renders the ground with `gizmos` (or none) on lavapipe at the golden's camera.
fn render_overlay(
    device: &regia_render::RenderDevice,
    renderer: &mut regia_render::FrameRenderer,
    gizmos: Option<Arc<regia_render::gizmos::GizmoFrame>>,
) -> Vec<u8> {
    let ground = golden_ground();
    let indices = [0_u32, 2, 1, 0, 3, 2];
    let material = regia_render::LabMaterial {
        base_color: [0.35, 0.36, 0.38, 1.0],
        roughness: 0.8,
        ..regia_render::LabMaterial::default()
    };
    let mesh = regia_render::FrameMesh {
        is_static: false,
        mesh_key: 1,
        material_key: 1,
        object_key: 1,
        vertices: &ground,
        indices: &indices,
        material: &material,
        model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: regia_render::ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    };
    let scene = regia_render::FrameScene {
        width: GOLDEN_W,
        height: GOLDEN_H,
        eye: [0.7, 1.7, 2.6],
        target: [0.7, 0.35, -0.5],
        fov_y: 0.85,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.4,
        double_sided: true,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: regia_render::DirectionalLightUniform::new([-0.4, -1.0, -0.3], [1.0, 1.0, 1.0], 1.2),
        local_lights: &[],
        quality: regia_render::FrameQuality::LAB,
        meshes: vec![mesh],
    };
    renderer.set_gizmos(gizmos);
    renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("render: {err}"))
        .pixels
}

fn golden_luma(px: &[u8; 4]) -> f64 {
    0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2])
}

/// Global luma SSIM (Wang, Bovik, Sheikh, Simoncelli, IEEE TIP 2004), as the gizmo golden uses.
fn golden_ssim(a: &[u8], b: &[u8]) -> f64 {
    let la: Vec<f64> = a.as_chunks::<4>().0.iter().map(golden_luma).collect();
    let lb: Vec<f64> = b.as_chunks::<4>().0.iter().map(golden_luma).collect();
    let n = la.len() as f64;
    let (ma, mb) = (la.iter().sum::<f64>() / n, lb.iter().sum::<f64>() / n);
    let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
    for (x, y) in la.iter().zip(&lb) {
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
        cov += (x - ma) * (y - mb);
    }
    let (va, vb, cov) = (va / n, vb / n, cov / n);
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

fn golden_png(path: &std::path::Path, pixels: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), GOLDEN_W, GOLDEN_H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)?;
    writer.finish()?;
    Ok(())
}

fn golden_read(path: &std::path::Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    Some(buf)
}

/// Every toggle on (gait, foot placement, IK): a FABRIK hexapod (cone and hinge limits) walking
/// and the demo biped planting its feet beside it, drawn through `regia_render`'s gizmo pass on
/// lavapipe. The overlay must change the frame, and the frame must match the golden
/// `docs/evidence/gizmos/locomotion_all.png` (global luma SSIM >= 0.98; judged by eye when
/// blessed). Bless with `REGIA_BLESS=1` on the lavapipe reference only; `REGIA_PREVIEW=1`
/// writes `<temp>/locomotion-gizmo-preview/` and skips the compare. A hardware adapter skips
/// the compare (the golden is lavapipe's).
#[test]
fn locomotion_overlay_matches_the_lavapipe_golden() {
    let device = regia_render::RenderDevice::headless().expect("headless device (lavapipe)");
    let mut app = app(Slope::flat());
    spawn_hexapod(&mut app, true);
    let biped = spawn_biped(&mut app);
    app.world_mut()
        .get_mut::<Transform>(biped)
        .expect("biped transform")
        .translation
        .x = 1.6;
    set_enabled(&mut app, true);
    for _ in 0..48 {
        app.update();
    }
    let s = stats(&app);
    // Every sub-toggle drew: the walker (gait), the planter's probes (foot placement), and the
    // legs of both with the FABRIK limits (IK).
    assert!(
        s.walkers == 1 && s.planters == 1 && s.probes == 2 && s.chains == 8,
        "{s:?}"
    );
    assert!(
        s.cone_limits == 12 && s.hinge_limits == 12 && s.swing_arcs > 0,
        "{s:?}"
    );
    let gizmos = Arc::clone(&app.world().resource::<regia_render::ExtractedFrame>().gizmos);
    assert!(gizmos.segment_count() > 0);

    let mut renderer = regia_render::FrameRenderer::new();
    let without = render_overlay(&device, &mut renderer, None);
    let with = render_overlay(&device, &mut renderer, Some(gizmos));
    let moved = with
        .as_chunks::<4>()
        .0
        .iter()
        .zip(without.as_chunks::<4>().0)
        .filter(|(a, b)| a.iter().zip(*b).take(3).any(|(p, q)| p.abs_diff(*q) > 12))
        .count();
    eprintln!("locomotion overlay pixels: {moved}");
    assert!(moved > 3_000, "the overlay barely drew ({moved} changed pixels)");

    let preview = std::env::temp_dir().join("locomotion-gizmo-preview");
    for (name, pixels) in [("locomotion_all.png", &with), ("locomotion_none.png", &without)] {
        if let Err(err) = golden_png(&preview.join(name), pixels) {
            eprintln!("preview {name} not written: {err}");
        }
    }
    let path = golden_path();
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() == Ok("1") {
        golden_png(&path, &with).expect("bless the golden");
        return;
    }
    if regia_core::brand::env_var("REGIA_PREVIEW").as_deref() == Ok("1") {
        return;
    }
    if device.adapter.get_info().device_type != wgpu::DeviceType::Cpu {
        eprintln!("hardware GPU: golden is blessed on lavapipe; compare skipped");
        return;
    }
    let Some(golden) = golden_read(&path) else {
        panic!(
            "golden {} is missing: the lead blesses it with REGIA_BLESS=1 on the lavapipe reference",
            path.display()
        );
    };
    assert_eq!(golden.len(), with.len(), "golden size");
    let score = golden_ssim(&with, &golden);
    eprintln!("locomotion_all SSIM {score:.4}");
    assert!(
        score >= GOLDEN_SSIM_MIN,
        "locomotion_all SSIM {score:.4} below {GOLDEN_SSIM_MIN}"
    );
}
