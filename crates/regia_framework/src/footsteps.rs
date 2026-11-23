// SPDX-License-Identifier: MPL-2.0
//! Footsteps from animation foot events (ADR-2442, `book/agents/audio-ambience.md`).
//!
//! An animation notify whose name starts with [`regia_audio::FootstepBank::notify_prefix`]
//! (`Footstep_L`, `Footstep_R`) on an entity with a [`FootstepWalker`] becomes one step: the
//! surface is the walker's override, else the host's [`FootstepSurfaceProbe`], else the dominant
//! terrain layer under the foot (mapped through `FootstepBank::layer_surfaces`), else the bank's
//! default; the speed is the walker's own when set, else the distance between its last two steps
//! over the time between them; the bank picks a variant that is not the walker's previous one on
//! that surface and plays it at the foot. Nothing runs without a `FootstepBank` resource.

use std::collections::HashMap;

use regia_animation::events::AnimNotify;
use regia_app::{App, Plugin, Update};
use regia_audio::{Audio, FootstepBank, FootstepEvent, SoundAssets};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_scene::GlobalTransform;
use regia_terrain::Terrain;

/// A character whose foot notifies play footsteps.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct FootstepWalker {
    /// Mass, kg (heavier steps are louder and lower).
    pub weight_kg: f32,
    /// Ground speed to use, m/s; `None` measures it from the steps.
    pub speed_m_per_s: Option<f32>,
    /// Surface to use whatever is under the foot (a wooden bridge over grass); `None` looks it up.
    pub surface: Option<String>,
    /// Height of the foot below the walker's origin, metres (0 for an origin at the feet).
    pub foot_offset_m: f32,
}

impl Default for FootstepWalker {
    /// 75 kg, measured speed, looked-up surface, origin at the feet.
    fn default() -> Self {
        Self {
            weight_kg: 75.0,
            speed_m_per_s: None,
            surface: None,
            foot_offset_m: 0.0,
        }
    }
}

/// A host's surface lookup (a physics material under the foot, a road mask): world position to
/// surface name. It wins over the terrain layer; `None` from it falls through to the terrain.
#[derive(Resource)]
pub struct FootstepSurfaceProbe(pub Box<dyn Fn(Vec3) -> Option<String> + Send + Sync>);

/// The dominant terrain layer under `(x, z)`: the layer whose slope and altitude weight is the
/// highest at that point of the first terrain that covers it. `None` off every terrain.
#[must_use]
pub fn terrain_layer_at<'a>(
    terrains: impl IntoIterator<Item = &'a Terrain>,
    x: f32,
    z: f32,
) -> Option<&'a str> {
    for terrain in terrains {
        let map = &terrain.map;
        // A map is centred on its grid origin (`TerrainMap::world_to_grid`, ADR-8179).
        let ([x0, z0], [x1, z1]) = (map.world_min(), map.world_max());
        if !((x0..=x1).contains(&x) && (z0..=z1).contains(&z)) {
            continue;
        }
        let normal = map.normal_at(x, z);
        let slope = normal[1].clamp(-1.0, 1.0).acos();
        let altitude = map.sample_bilinear(x, z);
        let weights = terrain.layers.weights(slope, altitude);
        let best = weights
            .iter()
            .enumerate()
            .filter(|(_, w)| w.is_finite() && **w > 0.0)
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)?;
        return terrain.layers.layer(best).map(|l| l.name.as_str());
    }
    None
}

/// The last step of each walker: where and when.
#[derive(Resource, Debug, Default)]
pub struct FootstepTracker {
    last: HashMap<Entity, (Vec3, f64)>,
    /// Foot notifies turned into steps since start.
    pub steps: u64,
    /// Foot notifies ignored (no `FootstepWalker` on the entity, or no bank set for any surface).
    pub ignored: u64,
}

/// The ground speed between two steps: distance over time, `None` for the first step, a gap
/// over 2 s (the walker stopped) or a zero interval.
#[must_use]
pub fn stride_speed(previous: Option<(Vec3, f64)>, position: Vec3, now: f64) -> Option<f32> {
    let (at, then) = previous?;
    let dt = (now - then) as f32;
    if !(1e-3..=2.0).contains(&dt) {
        return None;
    }
    let d = Vec3::new(position.x - at.x, 0.0, position.z - at.z).length();
    // A step covers half a stride: both feet move between two notifies of one foot, but the
    // notifies alternate feet, so the body moved `d` in `dt`.
    let speed = d / dt;
    speed.is_finite().then_some(speed)
}

/// What [`play_footsteps`] plays through: the bank, the mixer and the registered sounds.
type FootstepOutput<'w> = (
    Option<ResMut<'w, FootstepBank>>,
    Option<ResMut<'w, Audio>>,
    Option<Res<'w, SoundAssets>>,
);

/// `Update` system: turns foot notifies into footsteps (inert without a [`FootstepBank`]).
pub fn play_footsteps(
    mut notifies: MessageReader<AnimNotify>,
    time: Res<regia_app::Time>,
    (bank, audio, sounds): FootstepOutput<'_>,
    probe: Option<Res<FootstepSurfaceProbe>>,
    mut tracker: ResMut<FootstepTracker>,
    walkers: Query<(&FootstepWalker, Option<&GlobalTransform>)>,
    terrains: Query<&Terrain>,
) {
    let (Some(mut bank), Some(mut audio)) = (bank, audio) else {
        notifies.clear();
        return;
    };
    let _span = tracing::trace_span!("play_footsteps").entered();
    let now = time.elapsed().as_secs_f64();
    for notify in notifies.read() {
        if !notify.name.starts_with(bank.notify_prefix.as_str()) {
            continue;
        }
        let Ok((walker, global)) = walkers.get(notify.entity) else {
            tracker.ignored += 1;
            continue;
        };
        let origin = global.map(GlobalTransform::translation).unwrap_or_default();
        let position = origin - Vec3::new(0.0, walker.foot_offset_m, 0.0);
        let surface = walker
            .surface
            .clone()
            .or_else(|| probe.as_ref().and_then(|p| (p.0)(position)))
            .or_else(|| {
                terrain_layer_at(terrains.iter(), position.x, position.z)
                    .map(|layer| bank.surface_for_layer(layer).to_string())
            });
        let measured = stride_speed(tracker.last.get(&notify.entity).copied(), position, now);
        tracker.last.insert(notify.entity, (position, now));
        let event = FootstepEvent {
            walker: notify.entity.to_bits(),
            position,
            surface,
            speed_m_per_s: walker
                .speed_m_per_s
                .or(measured)
                .unwrap_or(bank.walk_speed_m_per_s),
            weight_kg: walker.weight_kg,
        };
        match bank.play(&mut audio, sounds.as_deref(), &event) {
            Ok(Some(_)) => tracker.steps += 1,
            Ok(None) => tracker.ignored += 1,
            Err(error) => {
                tracker.ignored += 1;
                tracing::warn!(target: "regia_framework", "footstep not played: {error}");
            }
        }
    }
    let alive: Vec<Entity> = tracker
        .last
        .keys()
        .copied()
        .filter(|e| walkers.contains(*e))
        .collect();
    tracker.last.retain(|e, _| alive.contains(e));
}

/// Adds [`play_footsteps`] and the [`FootstepTracker`]. `regia::DefaultPlugins` includes it;
/// the system does nothing until a [`FootstepBank`] resource exists.
pub struct FootstepPlugin;

impl Plugin for FootstepPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FootstepTracker>()
            .add_message::<AnimNotify>()
            .add_systems(Update, play_footsteps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings, FootstepSet, SoundClip};
    use regia_scene::Transform;

    fn app_with_bank() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins((AudioPlugin, FootstepPlugin));
        let mut bank = FootstepBank::new();
        let mut sounds = SoundAssets::default();
        for surface in ["grass", "gravel"] {
            let names: Vec<String> = (0..4).map(|i| format!("{surface}_{i}")).collect();
            for name in &names {
                let clip = SoundClip::new(vec![0.1; 480], 48_000).expect("a clip");
                sounds.add(name.clone(), regia_audio::SoundAsset::from_clip(&clip));
            }
            bank.sets.insert(surface.into(), FootstepSet::new(names));
        }
        bank.default_surface = "gravel".into();
        app.insert_resource(sounds);
        app.insert_resource(bank);
        app
    }

    fn notify(app: &mut App, entity: Entity, name: &str) {
        app.world_mut().write_message(AnimNotify {
            entity,
            name: name.into(),
            payload: None,
        });
    }

    #[test]
    fn foot_notifies_play_surface_steps_without_immediate_repeats() {
        let mut app = app_with_bank();
        let walker = app
            .world_mut()
            .spawn((
                FootstepWalker {
                    surface: Some("grass".into()),
                    ..FootstepWalker::default()
                },
                Transform::IDENTITY,
                GlobalTransform::IDENTITY,
            ))
            .id();
        let mut last = None;
        for i in 0..40 {
            notify(
                &mut app,
                walker,
                if i % 2 == 0 { "Footstep_L" } else { "Footstep_R" },
            );
            notify(&mut app, walker, "Jump");
            app.update();
            let bank = app.world().resource::<FootstepBank>();
            let play = bank.recent().last().cloned().expect("a step");
            assert_eq!(play.surface, "grass");
            assert!(play.started, "the registered variant played");
            assert_ne!(Some(play.variant), last, "step {i} repeated its variant");
            last = Some(play.variant);
        }
        let tracker = app.world().resource::<FootstepTracker>();
        assert_eq!(tracker.steps, 40, "only the foot notifies are steps");
        assert_eq!(app.world().resource::<FootstepBank>().played(), 40);
    }

    #[test]
    fn a_notify_without_a_walker_or_a_bank_is_ignored() {
        let mut app = app_with_bank();
        let stranger = app.world_mut().spawn(GlobalTransform::IDENTITY).id();
        notify(&mut app, stranger, "Footstep_L");
        app.update();
        assert_eq!(app.world().resource::<FootstepTracker>().ignored, 1);
        app.world_mut().remove_resource::<FootstepBank>();
        let walker = app.world_mut().spawn(FootstepWalker::default()).id();
        notify(&mut app, walker, "Footstep_L");
        app.update();
        assert_eq!(app.world().resource::<FootstepTracker>().steps, 0);
    }

    #[test]
    fn the_host_probe_picks_the_surface_and_unknown_surfaces_fall_back() {
        let mut app = app_with_bank();
        app.insert_resource(FootstepSurfaceProbe(Box::new(|p: Vec3| {
            (p.x > 0.0).then(|| "grass".to_string()).or(Some("ice".into()))
        })));
        let east = app
            .world_mut()
            .spawn((
                FootstepWalker::default(),
                GlobalTransform(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)).compute_affine()),
            ))
            .id();
        let west = app
            .world_mut()
            .spawn((
                FootstepWalker::default(),
                GlobalTransform(Transform::from_translation(Vec3::new(-5.0, 0.0, 0.0)).compute_affine()),
            ))
            .id();
        notify(&mut app, east, "Footstep_L");
        notify(&mut app, west, "Footstep_L");
        app.update();
        let bank = app.world().resource::<FootstepBank>();
        let steps: Vec<_> = bank.recent().map(|p| (p.surface.clone(), p.fallback)).collect();
        assert_eq!(
            steps,
            vec![("grass".to_string(), false), ("gravel".to_string(), true)]
        );
    }

    #[test]
    fn stride_speed_is_distance_over_time() {
        assert_eq!(stride_speed(None, Vec3::ZERO, 1.0), None);
        let previous = Some((Vec3::ZERO, 1.0));
        let speed = stride_speed(previous, Vec3::new(0.7, 0.3, 0.0), 1.5).expect("a speed");
        assert!((speed - 1.4).abs() < 1e-4, "{speed}");
        assert_eq!(
            stride_speed(previous, Vec3::X, 4.0),
            None,
            "a long pause is a fresh start"
        );
        assert_eq!(stride_speed(previous, Vec3::X, 1.0), None);
    }
}
