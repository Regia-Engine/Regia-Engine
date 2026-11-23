// SPDX-License-Identifier: MPL-2.0
//! Cell prefetch for cutscenes in a streamed level (`docs/specs/78-world-streaming.md` § 7.2). A camera
//! following a shot list wants its next stretch of world loaded before it gets there, so while a
//! sequence plays a scripted streaming source sits where its camera will be [`StreamPrefetch::seconds`]
//! from now. Idle without a streamed level: a game that does not stream never gets the entity.

use regia_app::prelude::*;
use regia_app::{PostUpdate, PostUpdateSet};
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::components::Name;
use regia_scene::streaming::{LevelStreaming, OriginOffset, SourceKind, StreamingSource};

use super::player::{PlaybackStatus, SequencePlayer};
use crate::shots::player::ShotPlayer;

/// How far ahead a playing sequence pulls cells. A resource so a game can tune it.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct StreamPrefetch {
    /// Seconds ahead of the clock. `0` turns the prefetch off.
    pub seconds: f64,
    /// Added to the priority of the cells it wants. Changes apply to existing sources on the
    /// next `PostUpdate`, just like changes to `seconds`.
    pub priority: i32,
}

impl Default for StreamPrefetch {
    fn default() -> Self {
        Self {
            seconds: 3.0,
            priority: 100,
        }
    }
}

/// The source of one player: the entity with the [`SequencePlayer`], or `None` for the shot player.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencePrefetchSource(pub Option<Entity>);

/// Where each playing sequence will be, in world meters, and who owns it.
fn wanted_points(
    prefetch: &StreamPrefetch,
    players: &Query<(Entity, &SequencePlayer)>,
    shot_player: Option<&ShotPlayer>,
) -> Vec<(Option<Entity>, Vec3)> {
    let mut out = Vec::new();
    for (entity, player) in players {
        if player.status() == PlaybackStatus::Playing
            && let Some(shots) = player.shots()
            && let Some(sample) = shots.sample((player.time() + prefetch.seconds).min(shots.duration()))
        {
            out.push((Some(entity), sample.eye));
        }
    }
    if let Some(player) = shot_player
        && player.is_playing()
        && let Some(shots) = player.sequence()
        && let Some(sample) = shots.sample((player.time() + prefetch.seconds).min(shots.duration()))
    {
        out.push((None, sample.eye));
    }
    out.retain(|(_, eye)| eye.is_finite());
    out
}

/// Keeps one scripted source per playing sequence at its prefetch point.
fn update_prefetch_sources(
    prefetch: Res<StreamPrefetch>,
    streaming: Option<Res<LevelStreaming>>,
    origin: Option<Res<OriginOffset>>,
    players: Query<(Entity, &SequencePlayer)>,
    shot_player: Option<Res<ShotPlayer>>,
    mut sources: Query<(
        Entity,
        &SequencePrefetchSource,
        &mut Transform,
        &mut StreamingSource,
    )>,
    mut commands: Commands,
) {
    let active = prefetch.seconds > 0.0 && streaming.is_some_and(|s| s.enabled);
    let wanted = if active {
        wanted_points(&prefetch, &players, shot_player.as_deref())
    } else {
        Vec::new()
    };
    if wanted.is_empty() && sources.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("sequence_stream_prefetch").entered();
    let offset = origin
        .as_deref()
        .map_or(Vec3::ZERO, |o| Vec3::from_array(o.0.map(|axis| axis as f32)));
    let mut placed: Vec<Option<Entity>> = Vec::with_capacity(wanted.len());
    for (entity, source, mut transform, mut streaming_source) in &mut sources {
        match wanted.iter().find(|(owner, _)| *owner == source.0) {
            Some((owner, eye)) => {
                transform.translation = *eye - offset;
                streaming_source.priority = prefetch.priority;
                placed.push(*owner);
            }
            None => commands.entity(entity).despawn(),
        }
    }
    for (owner, eye) in &wanted {
        if !placed.contains(owner) {
            commands.spawn((
                Name::new("sequence prefetch"),
                Transform::from_translation(*eye - offset),
                SequencePrefetchSource(*owner),
                StreamingSource {
                    kind: SourceKind::Scripted,
                    priority: prefetch.priority,
                    view_bias: 0.0,
                    look_ahead: true,
                    ..StreamingSource::default()
                },
            ));
        }
    }
}

/// Adds the system and its settings.
pub(crate) fn install(app: &mut App) {
    app.init_resource::<StreamPrefetch>().add_systems(
        PostUpdate,
        update_prefetch_sources.before(PostUpdateSet::TransformPropagate),
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use regia_app::{MinimalPlugins, TimeUpdateStrategy};

    use super::*;
    use crate::sequencer::asset::fixtures::sequence;
    use crate::sequencer::player::SequencerPlugin;
    use crate::shots::ShotSequence;
    use crate::shots::asset::PathKey;
    use crate::shots::asset::fixtures as shots;

    /// A shot list flying 10 m a second along X, for 20 seconds.
    fn flight() -> ShotSequence {
        let mut shot = shots::shot("a", 20.0);
        shot.camera.path.keys = vec![
            PathKey {
                time: 0.0,
                position: [0.0, 5.0, 0.0],
            },
            PathKey {
                time: 20.0,
                position: [200.0, 5.0, 0.0],
            },
        ];
        shots::sequence(vec![shot])
    }

    fn source_at(app: &mut App) -> Option<Vec3> {
        let mut query = app
            .world_mut()
            .query_filtered::<&Transform, With<SequencePrefetchSource>>();
        query.iter(app.world()).next().map(|t| t.translation)
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
        app
    }

    // Constant, distinct points on either side of a hard cut make the lifecycle oracle
    // independent of the order in which the two players' clocks advance within PostUpdate.
    fn cut_flight(base: Vec3) -> ShotSequence {
        let mut before = shots::shot("before", 10.0);
        before.camera.path.keys = vec![PathKey {
            time: 0.0,
            position: base.to_array(),
        }];
        let mut after = shots::shot("after", 30.0);
        after.camera.path.keys = vec![PathKey {
            time: 0.0,
            position: (base + Vec3::new(100.0, 200.0, -300.0)).to_array(),
        }];
        shots::sequence(vec![before, after])
    }

    const FIRST: Vec3 = Vec3::new(10.0, 20.0, 30.0);
    const SECOND: Vec3 = Vec3::new(-40.0, 50.0, -60.0);
    const STANDALONE: Vec3 = Vec3::new(70.0, -80.0, 90.0);
    const CUT: Vec3 = Vec3::new(100.0, 200.0, -300.0);

    fn three_players() -> (App, Entity, Entity) {
        let mut app = app();
        let mut first = SequencePlayer::new(sequence(vec![], vec![])).with_shots(cut_flight(FIRST));
        first.play();
        let first = app.world_mut().spawn(first).id();
        let mut second = SequencePlayer::new(sequence(vec![], vec![])).with_shots(cut_flight(SECOND));
        second.play();
        let second = app.world_mut().spawn(second).id();
        {
            let mut standalone = app.world_mut().resource_mut::<ShotPlayer>();
            standalone.load(cut_flight(STANDALONE));
            standalone.play();
        }
        (app, first, second)
    }

    /// Check the complete mapping, including duplicates and sources missing their runtime
    /// component. Return source ids so survivors and clean recreation can be checked separately.
    fn assert_sources(
        app: &mut App,
        expected: &[(Option<Entity>, Vec3)],
        priority: i32,
    ) -> BTreeMap<Option<Entity>, Entity> {
        let mut query = app.world_mut().query::<(
            Entity,
            &SequencePrefetchSource,
            &Transform,
            Option<&StreamingSource>,
        )>();
        let mut owners = BTreeMap::new();
        for (entity, owner, transform, source) in query.iter(app.world()) {
            assert!(
                owners.insert(owner.0, entity).is_none(),
                "duplicate owner {owner:?}"
            );
            let (_, position) = expected
                .iter()
                .find(|(expected_owner, _)| *expected_owner == owner.0)
                .expect("no stale or unexpected owner");
            assert!(transform.translation.is_finite(), "{owner:?}");
            assert!(
                transform.translation.distance(*position) < 1.0e-4,
                "{owner:?}: {:?}, expected {position:?}",
                transform.translation
            );
            let source = source.expect("a prefetch entity is a streaming source");
            assert_eq!(source.kind, SourceKind::Scripted);
            assert!(source.look_ahead, "prefetch must not become the streaming focus");
            assert_eq!(source.view_bias, 0.0);
            assert_eq!(source.priority, priority, "{owner:?}");
        }
        assert_eq!(owners.len(), expected.len(), "one source per active owner");
        owners
    }

    #[test]
    fn owners_pause_seek_resume_stop_and_despawn_independently() {
        let (mut app, first, second) = three_players();
        app.insert_resource(LevelStreaming::default());
        app.world_mut().resource_mut::<LevelStreaming>().enabled = true;
        let all = [(Some(first), FIRST), (Some(second), SECOND), (None, STANDALONE)];
        app.update();
        let initial = assert_sources(&mut app, &all, 100);

        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .pause();
        app.update();
        let surviving = [(Some(second), SECOND), (None, STANDALONE)];
        let ids = assert_sources(&mut app, &surviving, 100);
        assert_eq!(ids[&Some(second)], initial[&Some(second)]);
        assert_eq!(ids[&None], initial[&None]);
        let paused_time = app.world().get::<SequencePlayer>(first).expect("first").time();
        let running_time = app.world().get::<SequencePlayer>(second).expect("second").time();
        app.update();
        assert_sources(&mut app, &surviving, 100);
        assert_eq!(
            app.world().get::<SequencePlayer>(first).expect("first").time(),
            paused_time
        );
        assert!(app.world().get::<SequencePlayer>(second).expect("second").time() > running_time);

        // Seek while paused, then resume beyond the cut. No other owner's entity changes.
        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .seek(11.0);
        app.update();
        assert_sources(&mut app, &surviving, 100);
        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .play();
        app.update();
        let after_cut = [
            (Some(first), FIRST + CUT),
            (Some(second), SECOND),
            (None, STANDALONE),
        ];
        let resumed = assert_sources(&mut app, &after_cut, 100);
        assert_ne!(resumed[&Some(first)], initial[&Some(first)]);
        assert_eq!(resumed[&Some(second)], initial[&Some(second)]);
        assert_eq!(resumed[&None], initial[&None]);

        // A backwards seek and a look-ahead crossing both update the same source.
        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .seek(1.0);
        app.update();
        assert_eq!(assert_sources(&mut app, &all, 100), resumed);
        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .seek(7.0);
        app.update();
        assert_eq!(assert_sources(&mut app, &after_cut, 100), resumed);

        assert!(app.world_mut().despawn(first));
        app.update();
        assert_eq!(assert_sources(&mut app, &surviving, 100), ids);
        app.world_mut().resource_mut::<ShotPlayer>().stop();
        app.update();
        let ids = assert_sources(&mut app, &[(Some(second), SECOND)], 100);
        assert_eq!(ids[&Some(second)], initial[&Some(second)]);
        app.world_mut()
            .get_mut::<SequencePlayer>(second)
            .expect("second")
            .stop();
        app.update();
        assert_sources(&mut app, &[], 100);
        app.update();
        assert_sources(&mut app, &[], 100);
    }

    #[test]
    fn streaming_and_prefetch_switches_recreate_all_active_owners() {
        let (mut app, first, second) = three_players();
        let all = [(Some(first), FIRST), (Some(second), SECOND), (None, STANDALONE)];
        app.update();
        assert_sources(&mut app, &[], 100);
        app.insert_resource(LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        });
        app.update();
        let mut previous = assert_sources(&mut app, &all, 100);

        for switch in 0..3 {
            match switch {
                0 => app.world_mut().resource_mut::<LevelStreaming>().enabled = false,
                1 => {
                    app.world_mut().remove_resource::<LevelStreaming>();
                }
                _ => app.world_mut().resource_mut::<StreamPrefetch>().seconds = 0.0,
            }
            app.update();
            assert_sources(&mut app, &[], 100);
            app.update();
            assert_sources(&mut app, &[], 100);
            match switch {
                0 => app.world_mut().resource_mut::<LevelStreaming>().enabled = true,
                1 => {
                    app.insert_resource(LevelStreaming {
                        enabled: true,
                        ..LevelStreaming::default()
                    });
                }
                _ => app.world_mut().resource_mut::<StreamPrefetch>().seconds = 3.0,
            }
            app.update();
            let recreated = assert_sources(&mut app, &all, 100);
            for (owner, entity) in &recreated {
                assert_ne!(*entity, previous[owner], "{owner:?} must be recreated");
            }
            app.update();
            assert_eq!(assert_sources(&mut app, &all, 100), recreated);
            previous = recreated;
        }

        app.world_mut().resource_mut::<StreamPrefetch>().seconds = 12.0;
        app.update();
        let ahead = [
            (Some(first), FIRST + CUT),
            (Some(second), SECOND + CUT),
            (None, STANDALONE + CUT),
        ];
        assert_eq!(assert_sources(&mut app, &ahead, 100), previous);
        app.world_mut().resource_mut::<StreamPrefetch>().seconds = 3.0;
        app.update();
        assert_eq!(assert_sources(&mut app, &all, 100), previous);
    }

    #[test]
    fn origin_changes_while_paused_and_resumed_apply_once_per_owner() {
        let (mut app, first, second) = three_players();
        app.insert_resource(LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        });
        let all = [(Some(first), FIRST), (Some(second), SECOND), (None, STANDALONE)];
        app.update();
        let initial = assert_sources(&mut app, &all, 100);
        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .pause();
        app.world_mut().resource_mut::<ShotPlayer>().pause();
        app.update();
        assert_sources(&mut app, &[(Some(second), SECOND)], 100);

        for offset in [
            Vec3::new(1024.0, -2048.0, 4096.0),
            Vec3::new(-4096.0, 1024.0, -2048.0),
        ] {
            app.insert_resource(OriginOffset(offset.to_array().map(f64::from)));
            app.update();
            let ids = assert_sources(&mut app, &[(Some(second), SECOND - offset)], 100);
            assert_eq!(ids[&Some(second)], initial[&Some(second)]);
            app.world_mut()
                .get_mut::<SequencePlayer>(first)
                .expect("first")
                .play();
            app.world_mut().resource_mut::<ShotPlayer>().play();
            app.update();
            let local = [
                (Some(first), FIRST - offset),
                (Some(second), SECOND - offset),
                (None, STANDALONE - offset),
            ];
            let resumed = assert_sources(&mut app, &local, 100);
            app.update();
            assert_eq!(assert_sources(&mut app, &local, 100), resumed);
            let live_offset = offset * 2.0;
            app.insert_resource(OriginOffset(live_offset.to_array().map(f64::from)));
            app.update();
            let local = [
                (Some(first), FIRST - live_offset),
                (Some(second), SECOND - live_offset),
                (None, STANDALONE - live_offset),
            ];
            assert_eq!(assert_sources(&mut app, &local, 100), resumed);
            app.update();
            assert_eq!(assert_sources(&mut app, &local, 100), resumed);
            app.world_mut()
                .get_mut::<SequencePlayer>(first)
                .expect("first")
                .pause();
            app.world_mut().resource_mut::<ShotPlayer>().pause();
            app.update();
            assert_sources(&mut app, &[(Some(second), SECOND - live_offset)], 100);
        }
        app.world_mut().remove_resource::<OriginOffset>();
        app.world_mut()
            .get_mut::<SequencePlayer>(first)
            .expect("first")
            .play();
        app.world_mut().resource_mut::<ShotPlayer>().play();
        app.update();
        assert_sources(&mut app, &all, 100);
    }

    #[test]
    fn priority_changes_reach_existing_sources_on_the_next_update() {
        let (mut app, first, second) = three_players();
        app.insert_resource(LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        });
        let all = [(Some(first), FIRST), (Some(second), SECOND), (None, STANDALONE)];
        app.update();
        let initial = assert_sources(&mut app, &all, 100);
        for priority in [-17, 250, 0] {
            app.world_mut().resource_mut::<StreamPrefetch>().priority = priority;
            app.update();
            assert_eq!(assert_sources(&mut app, &all, priority), initial);
        }
    }

    #[test]
    fn a_playing_sequence_pulls_the_cells_three_seconds_ahead() {
        let mut app = app();
        app.insert_resource(LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        });
        let mut player = SequencePlayer::new(sequence(vec![], vec![])).with_shots(flight());
        player.play();
        let entity = app.world_mut().spawn(player).id();
        for _ in 0..5 {
            app.update();
        }
        let time = app.world().get::<SequencePlayer>(entity).expect("player").time();
        assert!(time > 0.0, "the clock runs");
        let at = source_at(&mut app).expect("a source");
        // The last update placed it from the clock it had: within a step of `time + 3 s`.
        assert!(
            (at.x - (time as f32 + 3.0) * 10.0).abs() <= 1.0 + 1.0e-3,
            "{at} at {time}"
        );
        assert!((at.y - 5.0).abs() < 1.0e-3);

        // A rebase: local again, the point in the world unchanged.
        app.insert_resource(OriginOffset([1024.0, 0.0, 0.0]));
        app.update();
        let moved = source_at(&mut app).expect("a source");
        assert!(moved.x < -900.0, "{moved}");

        // Stopped: the source goes.
        app.world_mut()
            .get_mut::<SequencePlayer>(entity)
            .expect("player")
            .stop();
        app.update();
        app.update();
        assert!(source_at(&mut app).is_none());
    }

    /// A game that does not stream never gets the entity.
    #[test]
    fn without_a_streamed_level_nothing_spawns() {
        let mut app = app();
        let mut player = SequencePlayer::new(sequence(vec![], vec![])).with_shots(flight());
        player.play();
        app.world_mut().spawn(player);
        for _ in 0..3 {
            app.update();
        }
        assert!(source_at(&mut app).is_none());
    }
}
