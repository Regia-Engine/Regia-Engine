// SPDX-License-Identifier: MPL-2.0
//! Steady emitter sync of virtual voices allocates nothing.
//!
//! The emitters sit past `max_distance` on the default bus (`None` means World). The first sync
//! and the first tick build tracks and virtualise the voices. The next minute of syncs must not
//! allocate.

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use regia_audio::{
    Audio, AudioEmitter, AudioSettings, EmitterFrame, EmitterUpdate, ListenerFrame, SoundAsset, SoundClip,
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
fn virtual_emitters_allocate_nothing_after_warmup() {
    let mut audio = headless();
    audio.set_listener(ListenerFrame::default());
    let asset = tone();
    let emitter = AudioEmitter {
        autoplay: false,
        max_distance: 20.0,
        ..AudioEmitter::default()
    };
    let mut updates = Vec::new();
    for index in 1..=8 {
        let entity = match Entity::from_raw_u32(index) {
            Some(entity) => entity,
            None => panic!("entity {index}"),
        };
        match audio.start_emitter(entity, asset.clone(), &emitter, 0.0) {
            Ok(_) => {}
            Err(error) => panic!("start emitter: {error}"),
        }
        updates.push(EmitterUpdate {
            entity,
            emitter: emitter.clone(),
            frame: EmitterFrame {
                position: Vec3::new(0.0, 0.0, 10_000.0),
                ..EmitterFrame::default()
            },
        });
    }
    audio.sync_emitter_frames(&updates, 1.0 / 60.0, None, None, None);
    audio.tick(1.0 / 60.0);
    let before = allocations();
    for _ in 0..60 {
        audio.sync_emitter_frames(&updates, 1.0 / 60.0, None, None, None);
    }
    let allocs = allocations() - before;
    assert_eq!(
        allocs, 0,
        "60 syncs of 8 virtual emitters allocated {allocs} times"
    );
}
