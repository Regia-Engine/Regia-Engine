// SPDX-License-Identifier: MPL-2.0
//! ADR-3270: the contact-sound step allocates nothing in steady state (resting contacts that report
//! a force every step, and a rolling sphere whose roll loop keeps playing).
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `feedback_cost.rs` and `abilities_alloc.rs`; thread-local counts). The system runs on this
//! thread through `World::run_system`, measured against an empty system run the same way.

#![cfg(feature = "client")]
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_audio::{
    AudioPlugin, AudioSettings, ImpactBank, ImpactBankDef, ImpactSet, SoundAsset, SoundAssets, SoundClip,
};
use regia_core::math::Vec3;
use regia_framework::physics_audio::{PhysicsAudioState, physics_audio_step};
use regia_physics::{
    ActiveEvents, Collider, ColliderShape, PhysicsMaterialRef, PhysicsPlugin, RigidBody, Velocity,
};
use regia_scene::Transform;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

/// A placeholder tone (generated, not recorded).
fn tone() -> SoundAsset {
    let samples: Vec<f32> = (0..4800)
        .map(|i| (i as f32 / 48_000.0 * 220.0 * std::f32::consts::TAU).sin() * 0.2)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("a clip"))
}

fn sounding(shape: ColliderShape) -> Collider {
    Collider {
        shape,
        active_events: ActiveEvents::ALL,
        ..Collider::default()
    }
}

#[test]
fn the_contact_sound_step_allocates_nothing_in_steady_state() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    // No PhysicsAudioPlugin: the step runs by hand below, on this thread.
    app.add_plugins((PhysicsPlugin, AudioPlugin));
    app.init_resource::<PhysicsAudioState>();
    // The step reads destruction moments too; the plugin would register them.
    app.add_message::<regia_physics::destruction::DestructionFxMessage>();
    let mut sounds = SoundAssets::default();
    for name in ["hit_0", "hit_1", "scrape", "roll"] {
        sounds.add(name, tone());
    }
    app.insert_resource(sounds);
    let mut def = ImpactBankDef::default();
    def.materials.insert(
        "default".into(),
        ImpactSet {
            scrape: Some("scrape".into()),
            roll: Some("roll".into()),
            ..ImpactSet::new(vec!["hit_0".into(), "hit_1".into()])
        },
    );
    app.insert_resource(ImpactBank::new(def));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        sounding(ColliderShape::Box {
            half_extents: Vec3::new(500.0, 0.5, 500.0),
        }),
        PhysicsMaterialRef("default".into()),
    ));
    // Ten boxes resting on the floor (awake, so they report a force every step).
    for i in 0..10 {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32 * 2.0, 0.25, 5.0)),
            RigidBody {
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            sounding(ColliderShape::Box {
                half_extents: Vec3::splat(0.25),
            }),
        ));
    }
    // A sphere rolling without slipping along +x: its roll loop keeps playing at a steady level.
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 0.25, -5.0)),
        RigidBody {
            can_sleep: false,
            ..RigidBody::dynamic()
        },
        sounding(ColliderShape::Sphere { radius: 0.25 }),
        Velocity {
            linear: Vec3::new(2.0, 0.0, 0.0),
            angular: Vec3::new(0.0, 0.0, -8.0),
        },
    ));
    let step_id = app.world_mut().register_system(physics_audio_step);
    let empty_id = app.world_mut().register_system(|| {});
    let measure = |app: &mut App| {
        app.update();
        let before = allocations();
        app.world_mut()
            .run_system(empty_id)
            .expect("the empty system runs");
        let empty = allocations() - before;
        let before = allocations();
        app.world_mut().run_system(step_id).expect("the step runs");
        (allocations() - before).saturating_sub(empty)
    };
    // Warm-up: the contacts settle, the pair table and scratch buffers reach their size.
    for _ in 0..120 {
        measure(&mut app);
    }
    let state = app.world().resource::<PhysicsAudioState>();
    assert!(
        state.pairs().iter().any(|p| p.voice.is_some()),
        "the roll loop plays: {:?}",
        state.pairs()
    );
    let contacts_before = state.stats.contacts;
    let mut total = 0;
    for _ in 0..240 {
        total += measure(&mut app);
    }
    let state = app.world().resource::<PhysicsAudioState>();
    assert!(
        state.stats.contacts >= contacts_before + 240 * 10,
        "the resting contacts kept reporting: {:?}",
        state.stats
    );
    assert_eq!(total, 0, "allocations in 240 steady steps: {total}");
}
