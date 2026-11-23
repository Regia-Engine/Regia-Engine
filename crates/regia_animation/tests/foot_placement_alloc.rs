// SPDX-License-Identifier: MPL-2.0
//! Steady-state allocation of pose-mode `FootPlacement`.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `param_apply_alloc.rs`). Zero allocations per frame is owed to animation-11: until that
//! lands, `FootPlanter::plan` builds its leg list every call. This test only requires the
//! count to be the same on settled frames.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{FootLegDesc, FootPlacement, GroundHit, GroundProbe, GroundProbes};
use regia_animation::{AnimationPlugin, Animator};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
use regia_ecs::prelude::Entity;
use regia_scene::Transform;

/// Counts on this thread. A parallel schedule runs systems on workers whose allocations this
/// counter cannot see, so the total changes as the executor moves work.
fn pin_schedules(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

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

struct Flat;

impl GroundProbe for Flat {
    fn probe(&self, from: Vec3, _down: Vec3, _max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        Some(GroundHit {
            point: Vec3::new(from.x, 0.0, from.z),
            normal: Vec3::Y,
        })
    }
}

#[test]
fn foot_placement_allocates_nothing_after_warmup() {
    let skeleton = Arc::new(
        demo_humanoid(&DemoRig {
            naming: RigNaming::Game,
            ..DemoRig::default()
        })
        .expect("demo rig"),
    );
    let gait = DemoGait::standard()
        .into_iter()
        .find(|gait| gait.name == "walk")
        .expect("walk");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("clip"),
    );
    let mut placement = FootPlacement::default();
    placement.legs = vec![
        FootLegDesc {
            hip: "thigh_l".into(),
            knee: "calf_l".into(),
            ankle: "foot_l".into(),
            toe: None,
            pole_offset: Vec3::ZERO,
        },
        FootLegDesc {
            hip: "thigh_r".into(),
            knee: "calf_r".into(),
            ankle: "foot_r".into(),
            toe: None,
            pole_offset: Vec3::ZERO,
        },
    ];
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.insert_resource(GroundProbes::new(Flat));
    pin_schedules(&mut app);
    let animator = Animator::single(clip, true);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator, placement))
        .id();

    // Warm up, then freeze the clip and stay there until a one-time buffer grow has
    // already happened. Settled frames sample one pose, so the count must not change.
    for _ in 0..60 {
        app.update();
    }
    // Schedules created on the first frames join the same executor before the frozen window.
    pin_schedules(&mut app);
    app.world_mut().get_mut::<Animator>(rig).expect("animator").speed = 0.0;
    for _ in 0..60 {
        app.update();
    }
    let mut counts = [0usize; 4];
    for slot in &mut counts {
        let before = allocations();
        app.update();
        *slot = allocations() - before;
    }
    assert!(
        counts.iter().all(|count| *count == counts[0]),
        "settled frames allocated {counts:?}; the count must be constant. \
         Zero per frame is owed to animation-11 (FootPlanter::plan still builds its leg list)"
    );
}

/// A 0.18 m step under the front of both feet (the toes are over the higher tread).
struct Step;

impl GroundProbe for Step {
    fn probe(&self, from: Vec3, _down: Vec3, _max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let y = if from.z < -0.1 { 0.18 } else { 0.0 };
        Some(GroundHit {
            point: Vec3::new(from.x, y, from.z),
            normal: Vec3::Y,
        })
    }
}

/// Allocations on four settled frames of a rig standing over a step, with or without heel and
/// toe probes (ADR-8505).
fn settled_counts(shape: bool) -> [usize; 4] {
    use regia_animation::foot_ik::FootShapeSettings;
    let skeleton = Arc::new(
        demo_humanoid(&DemoRig {
            naming: RigNaming::Game,
            ..DemoRig::default()
        })
        .expect("demo rig"),
    );
    let gait = DemoGait::standard()
        .into_iter()
        .find(|gait| gait.name == "walk")
        .expect("walk");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("clip"),
    );
    let mut placement = FootPlacement::default();
    placement.legs = ["l", "r"]
        .map(|side| FootLegDesc {
            hip: format!("thigh_{side}"),
            knee: format!("calf_{side}"),
            ankle: format!("foot_{side}"),
            toe: Some(format!("ball_{side}")),
            pole_offset: Vec3::ZERO,
        })
        .to_vec();
    if shape {
        placement.foot_shape = Some(FootShapeSettings::default());
    }
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.insert_resource(GroundProbes::new(Step));
    pin_schedules(&mut app);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(clip, true),
            placement,
        ))
        .id();
    for _ in 0..60 {
        app.update();
    }
    pin_schedules(&mut app);
    app.world_mut().get_mut::<Animator>(rig).expect("animator").speed = 0.0;
    for _ in 0..60 {
        app.update();
    }
    let mut counts = [0usize; 4];
    for slot in &mut counts {
        let before = allocations();
        app.update();
        *slot = allocations() - before;
    }
    counts
}

/// The heel and toe path (probes, edge bisection, stair rule, the buffered pose apply) adds no
/// allocation to a settled frame: the count is constant and no higher than the ankle-probe path's.
#[test]
fn heel_and_toe_probes_add_no_allocations() {
    let plain = settled_counts(false);
    let shaped = settled_counts(true);
    println!("foot_placement_alloc settled frames: ankle probe {plain:?}, heel and toe {shaped:?}");
    assert!(shaped.iter().all(|count| *count == shaped[0]), "{shaped:?}");
    assert!(
        shaped[0] <= plain[0],
        "heel and toe {shaped:?} vs ankle probe {plain:?}"
    );
}

/// Allocations on four settled frames of the walk rig over the step, with `placement` (or
/// none).
fn rig_counts(placement: Option<FootPlacement>) -> [usize; 4] {
    let skeleton = Arc::new(
        demo_humanoid(&DemoRig {
            naming: RigNaming::Game,
            ..DemoRig::default()
        })
        .expect("demo rig"),
    );
    let gait = DemoGait::standard()
        .into_iter()
        .find(|gait| gait.name == "walk")
        .expect("walk");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("clip"),
    );
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.insert_resource(GroundProbes::new(Step));
    pin_schedules(&mut app);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut rig = app
        .world_mut()
        .spawn((Transform::default(), binding, Animator::single(clip, true)));
    if let Some(placement) = placement {
        rig.insert(placement);
    }
    let rig = rig.id();
    for _ in 0..60 {
        app.update();
    }
    pin_schedules(&mut app);
    app.world_mut().get_mut::<Animator>(rig).expect("animator").speed = 0.0;
    for _ in 0..60 {
        app.update();
    }
    let mut counts = [0usize; 4];
    for slot in &mut counts {
        let before = allocations();
        app.update();
        *slot = allocations() - before;
    }
    counts
}

/// KI-AN-004, first part (ADR-10130): pose-mode `FootPlacement` adds no allocation to a settled
/// frame. The constant per-frame count the v0.6.0 notes listed is the app's own frame (the same
/// rig without the component allocates exactly as much).
#[test]
fn foot_placement_adds_no_allocation_to_a_settled_frame() {
    let mut placement = FootPlacement::default();
    placement.legs = ["l", "r"]
        .map(|side| FootLegDesc {
            hip: format!("thigh_{side}"),
            knee: format!("calf_{side}"),
            ankle: format!("foot_{side}"),
            toe: Some(format!("ball_{side}")),
            pole_offset: Vec3::ZERO,
        })
        .to_vec();
    let without = rig_counts(None);
    let with = rig_counts(Some(placement));
    println!("settled frames: without FootPlacement {without:?}, with {with:?}");
    assert_eq!(with, without, "FootPlacement's share of a settled frame");
}
