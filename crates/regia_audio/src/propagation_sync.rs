// SPDX-License-Identifier: MPL-2.0
//! The `AudioSync` systems of the propagation model (`docs/specs/16-audio.md` § 3c): copy the
//! level's medium, the listener's contact, interior volumes and emitter propagation settings into
//! [`Audio`] before the emitter sync, and forward ground-arrival rumbles to the gamepads after it.

use regia_ecs::prelude::*;
use regia_input::Gamepads;
use regia_scene::level::LevelSettings;
use regia_scene::{GlobalTransform, Transform};

use crate::audio::Audio;
use crate::coupling::{AudioInteriorVolume, AudioPropagation, InteriorFrame, ListenerCoupling};
use crate::medium::AudioMedium;
use crate::spatial::AudioListener;

/// `PostUpdate`/`AudioSync` system (before `audio_sync`): derives the medium from
/// `LevelSettings` whenever it changes, and copies the listener's [`ListenerCoupling`], the
/// [`AudioInteriorVolume`]s and every emitter's [`AudioPropagation`] into [`Audio`].
#[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
pub fn audio_propagation_sync(
    level: Option<Res<LevelSettings>>,
    mut audio: ResMut<Audio>,
    listeners: Query<(Entity, Option<&ListenerCoupling>), With<AudioListener>>,
    interiors: Query<(
        Entity,
        &AudioInteriorVolume,
        Option<&GlobalTransform>,
        Option<&Transform>,
    )>,
    emitters: Query<(Entity, &AudioPropagation)>,
) {
    let _span = tracing::debug_span!("audio_propagation_sync").entered();
    if let Some(level) = level.as_ref()
        && level.is_changed()
    {
        audio.set_medium(AudioMedium::from_level(
            level.atmosphere.preset,
            &level.audio_medium,
        ));
    }
    // Player 0 (the lowest listener entity, as in `audio_sync`) carries the contact.
    if let Some((_, coupling)) = listeners.iter().min_by_key(|(entity, _)| *entity) {
        match coupling {
            Some(coupling) if coupling == audio.listener_coupling() => {}
            Some(coupling) => audio.set_listener_coupling(coupling.clone()),
            None if *audio.listener_coupling() == ListenerCoupling::default() => {}
            None => audio.set_listener_coupling(ListenerCoupling::default()),
        }
    }
    let mut interior_count = 0usize;
    let mut interiors_differ = false;
    for (entity, volume, global, local) in interiors.iter() {
        let (center, rotation) = crate::plugin::pose(global, local);
        let same = audio.interiors().get(interior_count).is_some_and(|frame| {
            frame.entity == entity
                && frame.center == center
                && frame.rotation == rotation
                && frame.volume == *volume
        });
        if !same {
            interiors_differ = true;
        }
        interior_count += 1;
    }
    if interiors_differ || interior_count != audio.interiors().len() {
        let frames: Vec<InteriorFrame> = interiors
            .iter()
            .map(|(entity, volume, global, local)| {
                let (center, rotation) = crate::plugin::pose(global, local);
                InteriorFrame {
                    entity,
                    center,
                    rotation,
                    volume: volume.clone(),
                }
            })
            .collect();
        audio.set_interiors(frames);
    }
    // Unchanged emitters stay in the map. Cloning every ground name every frame was the cost.
    audio.sync_propagation_emitters(emitters.iter());
}

/// `PostUpdate`/`AudioSync` system (after `audio_sync`): forwards this frame's ground rumbles to
/// every connected gamepad through `regia_input` (a distant explosion is felt). A pad has one
/// motor command, so each pad receives only the strongest due rumble and that rumble's duration.
/// Equal strengths keep the first rumble.
#[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
pub fn ground_haptics(mut audio: ResMut<Audio>, gamepads: Option<ResMut<Gamepads>>) {
    let _span = tracing::debug_span!("ground_haptics").entered();
    let rumbles = audio.take_rumbles();
    let Some(mut gamepads) = gamepads else {
        return;
    };
    let Some(strongest) = rumbles
        .into_iter()
        .reduce(|a, b| if b.strength > a.strength { b } else { a })
    else {
        return;
    };
    let pads: Vec<u64> = gamepads.pads().map(|(id, _)| id).collect();
    for pad in pads {
        gamepads.rumble(
            pad,
            strongest.strength,
            crate::bus::safe_duration(strongest.duration_secs),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::AudioSettings;

    fn run_haptics(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(ground_haptics);
        schedule.run(world);
    }

    fn world_with_rumbles() -> World {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        audio.push_rumble_for_test(0.2, 0.1);
        audio.push_rumble_for_test(0.9, 0.4);
        let mut world = World::new();
        world.insert_resource(audio);
        world
    }

    #[test]
    fn ground_haptics_drains_the_queue_with_and_without_pads() {
        // Without a `Gamepads` resource a stale rumble is dropped, not carried to a later frame.
        let mut world = world_with_rumbles();
        run_haptics(&mut world);
        assert!(world.resource_mut::<Audio>().take_rumbles().is_empty());
        // With the resource but no connected pad the queue drains and nothing panics.
        let mut world = world_with_rumbles();
        world.init_resource::<Gamepads>();
        run_haptics(&mut world);
        assert!(world.resource_mut::<Audio>().take_rumbles().is_empty());
        run_haptics(&mut world);
    }

    #[test]
    fn ground_haptics_sends_only_the_strongest_rumble_and_its_duration() {
        let mut world = World::new();
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        // The longer weaker event must not replace the strongest one's duration.
        audio.push_rumble_for_test(0.2, 1.5);
        audio.push_rumble_for_test(0.9, 0.15);
        world.insert_resource(audio);
        let mut pads = Gamepads::default();
        pads.connect_pad(7, "pad");
        pads.connect_pad(8, "second");
        world.insert_resource(pads);
        run_haptics(&mut world);
        let queued = world.resource::<Gamepads>().queued_rumbles().to_vec();
        assert_eq!(queued.len(), 2, "one command per connected pad: {queued:?}");
        for (device, strength, duration) in &queued {
            assert!((*device == 7 || *device == 8), "{device}");
            assert!((*strength - 0.9).abs() < 1e-5, "{strength}");
            assert_eq!(*duration, crate::bus::safe_duration(0.15));
        }
        assert!(world.resource_mut::<Audio>().take_rumbles().is_empty());
    }
}
