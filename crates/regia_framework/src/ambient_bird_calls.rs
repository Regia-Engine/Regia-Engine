// SPDX-License-Identifier: MPL-2.0
//! Bird calls heard where the birds are seen (ADR-3024). `regia_vfx` flies the birds
//! ([`regia_vfx::AmbientBirds`]) and `regia_audio` plays the calls; the two are the same layer and
//! may not depend on each other, so the join lives here.
//!
//! When a one-shot [`AudioEmitter`] starts whose sound is one of a flock's
//! [`AmbientBirds::call_sounds`], within [`AmbientBirds::call_reach_m`] of the flock's anchor,
//! [`bind_bird_calls`] moves it onto the nearest visible bird of that flock and tags it
//! [`FollowAmbientBird`], so the call travels with the bird for as long as it plays. A call with no
//! visible bird (the flock faded out in rain or at night, as the calls themselves do) stays where
//! the audio put it and is counted as unbound.
//!
//! Opt-in: add [`AmbientBirdCallsPlugin`] next to `regia_vfx::VfxPlugin` and the audio plugin.

use regia_app::prelude::*;
use regia_audio::AudioEmitter;
use regia_scene::Transform;
use regia_vfx::ambient::{BirdFlockSim, step_ambient_birds};
use regia_vfx::{AmbientBirds, FollowAmbientBird};

/// How many bird calls were bound to a visible bird, and how many found none.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmbientBirdCalls {
    /// Calls moved onto a bird.
    pub bound: u64,
    /// Calls of a flock's sound, in reach, with no visible bird.
    pub unbound: u64,
}

/// New one-shot emitters that do not follow a bird yet: what [`bind_bird_calls`] looks at.
type NewCalls<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static AudioEmitter, &'static mut Transform),
    (Added<AudioEmitter>, Without<FollowAmbientBird>),
>;

/// `Update`, after the birds fly: binds each new one-shot call emitter to the nearest visible bird
/// of the flock whose call it is (see the module docs).
pub fn bind_bird_calls(
    mut added: NewCalls,
    flocks: Query<(Entity, &AmbientBirds, &BirdFlockSim)>,
    mut stats: ResMut<AmbientBirdCalls>,
    mut commands: Commands,
) {
    for (entity, emitter, mut transform) in &mut added {
        if emitter.looping || emitter.sound_name.is_empty() {
            continue;
        }
        let at = transform.translation;
        let mut matched = false;
        let mut best: Option<(Entity, usize, regia_core::math::Vec3, f32)> = None;
        for (flock, config, sim) in &flocks {
            if !config.call_sounds.contains(&emitter.sound_name)
                || sim.anchor().distance(at) > config.call_reach_m()
            {
                continue;
            }
            matched = true;
            if let Some((bird, position)) = sim.nearest_visible(at, sim.visible()) {
                let d2 = position.distance_squared(at);
                if best.is_none_or(|(.., best_d2)| d2 < best_d2) {
                    best = Some((flock, bird, position, d2));
                }
            }
        }
        match best {
            Some((flock, bird, position, _)) => {
                transform.translation = position;
                commands.entity(entity).insert(FollowAmbientBird {
                    flock,
                    bird: bird as u32,
                });
                stats.bound += 1;
            }
            None if matched => stats.unbound += 1,
            None => {}
        }
    }
}

/// Installs [`bind_bird_calls`] (add with `regia_vfx::VfxPlugin` and the audio plugin).
pub struct AmbientBirdCallsPlugin;

impl Plugin for AmbientBirdCallsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AmbientBirdCalls>()
            .add_systems(Update, bind_bird_calls.after(step_ambient_birds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_vfx::AmbientLifeSettings;
    use regia_vfx::ambient::AmbientLifePlugin;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(50),
        ));
        app.add_plugins((
            regia_app::MinimalPlugins,
            AmbientLifePlugin,
            AmbientBirdCallsPlugin,
        ));
        app.insert_resource(AmbientLifeSettings {
            enabled: true,
            ..AmbientLifeSettings::default()
        });
        app
    }

    fn call(sound: &str, at: Vec3, looping: bool) -> (AudioEmitter, Transform) {
        (
            AudioEmitter {
                sound_name: sound.into(),
                looping,
                ..AudioEmitter::default()
            },
            Transform::from_translation(at),
        )
    }

    #[test]
    fn a_call_is_heard_where_a_bird_is_seen() {
        let mut app = app();
        let anchor = Vec3::new(20.0, 0.0, -60.0);
        let flock = app
            .world_mut()
            .spawn((
                AmbientBirds {
                    call_sounds: vec!["bird_a".into(), "bird_b".into()],
                    ..AmbientBirds::flock()
                },
                Transform::from_translation(anchor),
            ))
            .id();
        for _ in 0..10 {
            app.update();
        }
        // The audio's scatter spot starts a call at a random point near the tree line.
        let heard = app
            .world_mut()
            .spawn(call("bird_b", anchor + Vec3::new(5.0, 8.0, 3.0), false))
            .id();
        let water = app.world_mut().spawn(call("stream", anchor, true)).id();
        let far = app
            .world_mut()
            .spawn(call("bird_a", anchor + Vec3::new(5_000.0, 0.0, 0.0), false))
            .id();
        app.update();
        let follow = *app
            .world()
            .get::<FollowAmbientBird>(heard)
            .expect("bound to a bird");
        assert_eq!(follow.flock, flock);
        for _ in 0..5 {
            app.update();
        }
        let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
        let bird = sim.position(follow.bird as usize).expect("bird");
        let at = app.world().get::<Transform>(heard).expect("call").translation;
        assert!(
            at.distance(bird) < 1.0e-4,
            "the call moves with its bird: {at} vs {bird}"
        );
        assert!(
            app.world().get::<FollowAmbientBird>(water).is_none(),
            "a loop is not a call"
        );
        assert!(
            app.world().get::<FollowAmbientBird>(far).is_none(),
            "out of reach"
        );
        let stats = *app.world().resource::<AmbientBirdCalls>();
        assert_eq!(stats, AmbientBirdCalls { bound: 1, unbound: 0 });
    }

    #[test]
    fn a_call_with_no_visible_bird_stays_put() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<AmbientLifeSettings>()
            .density_scale = 0.0;
        let anchor = Vec3::new(0.0, 0.0, -30.0);
        app.world_mut().spawn((
            AmbientBirds {
                call_sounds: vec!["bird_a".into()],
                ..AmbientBirds::flock()
            },
            Transform::from_translation(anchor),
        ));
        for _ in 0..5 {
            app.update();
        }
        let start = anchor + Vec3::new(1.0, 9.0, 2.0);
        let heard = app.world_mut().spawn(call("bird_a", start, false)).id();
        app.update();
        assert!(app.world().get::<FollowAmbientBird>(heard).is_none());
        assert_eq!(
            app.world().get::<Transform>(heard).expect("call").translation,
            start
        );
        assert_eq!(app.world().resource::<AmbientBirdCalls>().unbound, 1);
    }
}
