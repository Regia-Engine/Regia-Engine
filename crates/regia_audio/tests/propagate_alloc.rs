// SPDX-License-Identifier: MPL-2.0
//! A coupled emitter with an unchanged listener solves propagation without allocating.
//!
//! Machinery couples into the ground. The first syncs build the ground voice and the material
//! name. Later syncs of the same pose must reuse both.

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use regia_audio::{
    Audio, AudioEmitter, AudioPropagation, AudioSettings, EmitterCategory, EmitterFrame, EmitterUpdate,
    ListenerCoupling, ListenerFrame, SoundAsset, SoundClip,
};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn tone() -> SoundAsset {
    let samples = vec![0.2_f32; 2_400];
    let clip = match SoundClip::new(samples, 48_000) {
        Ok(clip) => clip,
        Err(error) => panic!("tone clip: {error}"),
    };
    SoundAsset::from_clip(&clip)
}

#[test]
fn steady_ground_coupling_allocates_nothing() {
    let mut audio = headless();
    audio.set_listener(ListenerFrame::default());
    audio.set_listener_coupling(ListenerCoupling::default());
    let entity = match Entity::from_raw_u32(1) {
        Some(entity) => entity,
        None => panic!("entity"),
    };
    let emitter = AudioEmitter {
        autoplay: false,
        max_distance: 80.0,
        ..AudioEmitter::default()
    };
    audio.set_emitter_propagation(entity, AudioPropagation::for_category(EmitterCategory::Machinery));
    match audio.start_emitter(entity, tone(), &emitter, 0.0) {
        Ok(_) => {}
        Err(error) => panic!("start emitter: {error}"),
    }
    let update = EmitterUpdate {
        entity,
        emitter,
        frame: EmitterFrame {
            position: Vec3::new(3.0, 0.0, 0.0),
            ..EmitterFrame::default()
        },
    };
    // Warm-up: the track, the ground voice and the first material name.
    audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
    audio.tick(1.0 / 60.0);
    audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
    let before = allocations();
    for _ in 0..120 {
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "120 steady propagation syncs allocated {allocs} times");
}
