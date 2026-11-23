// SPDX-License-Identifier: MPL-2.0
//! Spec 11 § 2 (spec conformance 03-11): the `audio_lifecycle` system pauses the output stream on
//! `Suspended` and restores it on `Resumed`, driven through `AppLifecycle` the way the platform
//! runner drives it (the existing suspend tests call `Audio::suspend_output` directly). Headless
//! offline backend: no device.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_audio::{Audio, AudioSettings, audio_lifecycle};
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_platform::{AppLifecycle, LifecycleEvent};

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    world.insert_resource(AppLifecycle::default());
    world
}

fn deliver(world: &mut World, event: LifecycleEvent) {
    world.resource_mut::<AppLifecycle>().last_event = Some(event);
    world.run_system_once(audio_lifecycle).expect("runs");
}

#[test]
fn suspended_pauses_the_output_and_resumed_restores_it() {
    let mut world = world();
    assert!(!world.resource::<Audio>().output_suspended());
    deliver(&mut world, LifecycleEvent::Started);
    assert!(!world.resource::<Audio>().output_suspended());

    deliver(&mut world, LifecycleEvent::Suspended);
    assert!(
        world.resource::<Audio>().output_suspended(),
        "Suspended pauses the output stream"
    );
    // LowMemory while suspended leaves the stream down.
    deliver(&mut world, LifecycleEvent::LowMemory);
    assert!(world.resource::<Audio>().output_suspended());

    deliver(&mut world, LifecycleEvent::Resumed);
    assert!(
        !world.resource::<Audio>().output_suspended(),
        "Resumed restores the output stream"
    );
}

#[test]
fn without_a_lifecycle_resource_the_system_is_inert() {
    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    world.run_system_once(audio_lifecycle).expect("runs");
    assert!(!world.resource::<Audio>().output_suspended());
}
