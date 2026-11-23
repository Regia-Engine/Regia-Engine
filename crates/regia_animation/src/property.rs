// SPDX-License-Identifier: MPL-2.0
//! Property animation: animating reflected fields of any component from a clip's property
//! tracks (doors, lights, cinematics; spec 17 § 4).
//!
//! Paths are `Component.field.subfield...`, where `Component` is the registered short type
//! name (for example `"Transform.translation.y"`). A curve sample is an `f32`. An `f32` leaf
//! stores that sample. An `i32` leaf stores `value.round() as i32` (the Rust cast: a finite
//! value outside `i32` saturates, and a non-finite value becomes 0). A `bool` leaf stores
//! `value != 0.0`, so any sample other than `0.0` — including negatives and NaN — writes
//! `true`. Any other leaf, including `u32` and `i64`, is left unchanged.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regia_app::{FixedUpdate, FixedUpdateSet, Time};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use tracing::warn;

use crate::clip::AnimationClip;

/// Short component name to type id, rebuilt when the registry's shape changes.
///
/// The generation is the registration count plus the number of registrations carrying
/// [`ReflectComponent`] data: `TypeRegistry::iter` reports its length, registrations are not
/// removed, but `register_type_data` adds data to an existing registration without changing
/// the count, and the data is what this map indexes. One world has its own cache, so parallel
/// tests do not share it (ADR-867 still picks the smallest full path when two components share
/// a short name).
#[derive(Resource, Default)]
struct ComponentNameCache {
    count: usize,
    with_reflect: usize,
    by_short: HashMap<String, std::any::TypeId>,
}

/// One sampled track. The clip `Arc` keeps the path without cloning it.
struct TrackSample {
    entity: Entity,
    clip: Arc<AnimationClip>,
    track: u32,
    value: f32,
}

/// Rows and reflected-component handles reused across fixed steps.
#[derive(Resource, Default)]
struct PropertyScratch {
    rows: Vec<TrackSample>,
    /// Component type of each row, filled from the name cache before the registry read.
    type_ids: Vec<Option<std::any::TypeId>>,
    /// Kept across steps. Registrations are not removed, so a handle stays valid.
    reflect: HashMap<std::any::TypeId, ReflectComponent>,
}

/// Most distinct property-track warnings one world remembers; past it, new ones stay silent so
/// worlds that churn animated entities cannot grow the set without bound.
const PROPERTY_WARN_CAP: usize = 1024;

/// What a property-track warning is about. With the entity (or, for `Leaf`, with no entity) and
/// the track path it names one warning.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum WarnKind {
    NoDot,
    Unknown,
    Missing,
    Leaf,
    Path,
}

/// Property-track warnings already issued in this world: one per entity and path (one per path
/// for an unsupported leaf type), bounded by `PROPERTY_WARN_CAP` (ADR-10131).
///
/// Kept per world, not per process: entity ids repeat between worlds, so a process-wide set
/// silenced a second world's first entity, and every other subsystem's warnings counted
/// against the same cap. Looking up a path that already warned allocates nothing.
#[derive(Resource, Default)]
struct PropertyWarnings {
    seen: HashMap<(Entity, WarnKind), HashSet<String>>,
    len: usize,
}

impl PropertyWarnings {
    /// True the first time `kind` is seen for `entity` and `path` in this world.
    fn first(&mut self, entity: Entity, kind: WarnKind, path: &str) -> bool {
        if self
            .seen
            .get(&(entity, kind))
            .is_some_and(|paths| paths.contains(path))
            || self.len >= PROPERTY_WARN_CAP
        {
            return false;
        }
        self.seen
            .entry((entity, kind))
            .or_default()
            .insert(path.to_owned());
        self.len += 1;
        true
    }
}

/// Animates reflected fields of the entity's own components from a clip's property tracks.
#[derive(Component, Clone, Debug)]
pub struct PropertyAnimator {
    /// The clip (property tracks only are read).
    pub clip: std::sync::Arc<AnimationClip>,
    /// Playback time.
    pub time: f32,
    /// Speed multiplier.
    pub speed: f32,
    /// Loop playback.
    pub looping: bool,
}

/// Advance every [`PropertyAnimator`] by one fixed step and apply its property tracks through
/// reflection. Runs as an exclusive system so it can address components by reflected type.
///
/// Scheduling (ADR-4653): `FixedUpdate` / [`FixedUpdateSet::Animation`]. Property animation is
/// simulation: inside `FixedUpdate`, [`Time::delta_secs`] is one fixed step, so every fixed
/// step advances the clock once and the value a kinematic consumer (the physics step) sees is
/// the same on every peer whatever the frame pacing. A frame covering several fixed steps
/// applies the tracks once per step, before the physics step reads `Transform`.
#[tracing::instrument(skip_all)]
pub fn apply_property_animators(world: &mut World) {
    let _span = tracing::debug_span!("apply_property_animators").entered();
    let delta = world.resource::<Time>().delta_secs();
    world.init_resource::<ComponentNameCache>();
    world.init_resource::<PropertyScratch>();
    world.init_resource::<PropertyWarnings>();
    let registry = world.resource::<AppTypeRegistry>().clone();
    {
        let registrations = registry.read();
        refresh_name_cache(world, &registrations);
    }

    let mut scratch = std::mem::take(&mut *world.resource_mut::<PropertyScratch>());
    scratch.rows.clear();
    scratch.type_ids.clear();

    // A parked layer member is not simulated (spec 37 § 13): its clip keeps its time.
    let mut query =
        world.query_filtered::<(Entity, &mut PropertyAnimator), Without<regia_scene::layers::LayerParked>>();
    for (entity, mut animator) in query.iter_mut(world) {
        animator.time = crate::clip::AnimationClip::advance(
            animator.time,
            delta * animator.speed,
            animator.clip.duration,
            animator.looping,
        )
        .0;
        let clip = Arc::clone(&animator.clip);
        for (track_index, track) in clip.property_tracks.iter().enumerate() {
            scratch.rows.push(TrackSample {
                entity,
                clip: Arc::clone(&clip),
                track: u32::try_from(track_index).unwrap_or(u32::MAX),
                value: track.curve.sample(animator.time),
            });
        }
    }

    let mut type_ids = std::mem::take(&mut scratch.type_ids);
    type_ids.clear();
    {
        let cache = world.resource::<ComponentNameCache>();
        type_ids.reserve(scratch.rows.len());
        for row in &scratch.rows {
            let id = row
                .clip
                .property_tracks
                .get(row.track as usize)
                .and_then(|track| track.path.split_once('.'))
                .and_then(|(name, _)| cache.by_short.get(name).copied());
            type_ids.push(id);
        }
    }
    {
        let registrations = registry.read();
        for id in type_ids.iter().flatten().copied() {
            if scratch.reflect.contains_key(&id) {
                continue;
            }
            if let Some(reflect) = registrations
                .get(id)
                .and_then(|r| r.data::<ReflectComponent>().cloned())
            {
                scratch.reflect.insert(id, reflect);
            }
        }
    }
    // `rows` and `reflect` are separate locals so applying can borrow a path and a handle together.
    let mut rows = std::mem::take(&mut scratch.rows);
    let reflect_map = std::mem::take(&mut scratch.reflect);
    let mut warnings = std::mem::take(&mut *world.resource_mut::<PropertyWarnings>());
    for (row, type_id) in rows.iter().zip(type_ids.iter().copied()) {
        let Some(track) = row.clip.property_tracks.get(row.track as usize) else {
            continue;
        };
        let reflect = type_id.and_then(|id| reflect_map.get(&id));
        apply_value(
            world,
            &mut warnings,
            type_id.is_some(),
            reflect,
            row.entity,
            &track.path,
            row.value,
        );
    }
    rows.clear();
    scratch.rows = rows;
    scratch.type_ids = type_ids;
    scratch.reflect = reflect_map;
    world.insert_resource(scratch);
    world.insert_resource(warnings);
}

/// Rebuilds the short-name map when the registry's registration shape has changed.
fn refresh_name_cache(world: &mut World, registry: &regia_ecs::reflect::TypeRegistry) {
    let count = registry.iter().size_hint().0;
    let with_reflect = registry
        .iter()
        .filter(|registration| registration.data::<ReflectComponent>().is_some())
        .count();
    let cache = world.resource::<ComponentNameCache>();
    if cache.count == count && cache.with_reflect == with_reflect {
        return;
    }
    let built = component_name_map(registry);
    let mut cache = world.resource_mut::<ComponentNameCache>();
    cache.by_short.clear();
    // The vec is sorted, so the first short name is the smallest full path (ADR-867).
    // `or_insert` keeps that first id when two components share a short name.
    for (short, id) in built {
        cache.by_short.entry(short).or_insert(id);
    }
    cache.count = count;
    cache.with_reflect = with_reflect;
}

/// Map short component names (for example `"Transform"`) to type ids, from the registry.
///
/// Sorted by short name, then full type path. The registry iterates in `TypeId` order, which
/// differs between builds, so two components sharing a short name would otherwise resolve to
/// a different one in debug and release (ADR-867). The first match is the smallest full path.
fn component_name_map(registry: &regia_ecs::reflect::TypeRegistry) -> Vec<(String, std::any::TypeId)> {
    #[cfg(test)]
    perf_work::note_name_map();
    let mut names: Vec<(String, &str, std::any::TypeId)> = registry
        .iter()
        .filter_map(|registration| {
            registration.data::<ReflectComponent>()?;
            let path = registration.type_info().type_path();
            let short = path.rsplit("::").next()?;
            Some((short.to_string(), path, registration.type_id()))
        })
        .collect();
    names.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    names.into_iter().map(|(short, _, id)| (short, id)).collect()
}

/// Apply one float value to `Component.field...` on an entity.
///
/// `known` is false when the component's short name is not in the name cache. `reflect` is the
/// component's reflect handle from the batch's single registry read. Each failure warns once
/// per entity and path in this world (`warnings`).
fn apply_value(
    world: &mut World,
    warnings: &mut PropertyWarnings,
    known: bool,
    reflect_component: Option<&ReflectComponent>,
    entity: Entity,
    path: &str,
    value: f32,
) {
    let Some((component, field_path)) = path.split_once('.') else {
        if warnings.first(entity, WarnKind::NoDot, path) {
            warn!(target: "regia_animation", "property track path {path:?} has no component part");
        }
        return;
    };
    if !known {
        if warnings.first(entity, WarnKind::Unknown, path) {
            warn!(
                target: "regia_animation",
                "property track path {path:?} component {component:?} is not registered"
            );
        }
        return;
    }
    let Some(reflect_component) = reflect_component else {
        return;
    };

    let Ok(world_entity) = world.get_entity_mut(entity) else {
        return;
    };
    // `EntityWorldMut` converts into the `FilteredEntityMut` the reflect API takes.
    let filtered: regia_ecs::ecs::world::FilteredEntityMut = world_entity.into();
    let Some(mut reflected) = reflect_component.reflect_mut(filtered) else {
        if warnings.first(entity, WarnKind::Missing, path) {
            warn!(target: "regia_animation", "entity {entity:?} has no {component} component");
        }
        return;
    };
    match reflected.reflect_path_mut(field_path) {
        Ok(field) => {
            if !apply_f32(field, value) && warnings.first(Entity::PLACEHOLDER, WarnKind::Leaf, path) {
                let leaf = field
                    .get_represented_type_info()
                    .map_or("unknown", |info| info.type_path());
                warn!(
                    target: "regia_animation",
                    "property track path {path:?} ends in a {leaf} field; only f32, i32 and bool are animated"
                );
            }
        }
        Err(error) => {
            if warnings.first(entity, WarnKind::Path, path) {
                warn!(
                    target: "regia_animation",
                    "property track path {path:?} failed on {component}: {error}"
                );
            }
        }
    }
}

/// Writes `value` onto an `f32`, `i32` or `bool` leaf. See the module docs for the
/// conversion. Returns false when the leaf is another type and nothing was written.
fn apply_f32(field: &mut dyn regia_ecs::reflect::PartialReflect, value: f32) -> bool {
    if field.represents::<f32>() {
        field.apply(&value);
    } else if field.represents::<i32>() {
        field.apply(&(value.round() as i32));
    } else if field.represents::<bool>() {
        field.apply(&(value != 0.0));
    } else {
        return false;
    }
    true
}

/// Counts name-map rebuilds and track-path clones (audit perf G, finding 3). Test-only:
/// production builds do not keep these counters.
#[cfg(test)]
mod perf_work {
    use std::cell::Cell;

    thread_local! {
        static NAME_MAPS: Cell<u64> = const { Cell::new(0) };
        static PATH_CLONES: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        NAME_MAPS.with(|c| c.set(0));
        PATH_CLONES.with(|c| c.set(0));
    }

    pub(crate) fn note_name_map() {
        NAME_MAPS.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(crate) fn note_path_clone() {
        PATH_CLONES.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(crate) fn name_maps() -> u64 {
        NAME_MAPS.with(Cell::get)
    }

    pub(crate) fn path_clones() -> u64 {
        PATH_CLONES.with(Cell::get)
    }
}

/// Register the property-animation system (called by [`crate::AnimationPlugin`]).
pub(crate) fn register(app: &mut regia_app::App) {
    app.add_systems(
        FixedUpdate,
        // Before the physics step pushes kinematic Transforms: a property-animated door is a
        // kinematic platform, and every fixed step must see the value the animator set on the
        // previous one (ADR-4653). The pose apply, gaze, springs and sockets still run later,
        // in `PostUpdateSet::AnimationApply`, so ADR-2396's order (property tracks before the
        // pose) is unchanged.
        apply_property_animators.in_set(FixedUpdateSet::Animation),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::PropertyTrack;
    use regia_scene::Transform;

    /// Counts `regia_animation` warnings whose message contains one of `needles`. Installed
    /// with `tracing::subscriber::with_default`, so it sees only this test thread's events.
    struct WarnCount {
        seen: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        needles: Vec<String>,
    }

    struct Message(String);

    impl tracing::field::Visit for Message {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for WarnCount {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed).max(1);
            tracing::span::Id::from_u64(id)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let meta = event.metadata();
            if *meta.level() != tracing::Level::WARN || meta.target() != "regia_animation" {
                return;
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            if self
                .needles
                .iter()
                .any(|needle| message.0.contains(needle.as_str()))
            {
                self.seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A fresh app with only the property system, stepping one fixed step per update, and one
    /// entity whose clip has a track at `path` (Static 1.0).
    fn app_with_track(path: &str) -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins);
        app.register_type::<Transform>();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_nanos(16_666_667),
        ));
        app.add_systems(regia_app::FixedUpdate, apply_property_animators);
        let mut clip = AnimationClip::empty("door", 2.0);
        clip.property_tracks.push(PropertyTrack {
            path: path.into(),
            curve: crate::clip::TrackCurve::Static(1.0),
        });
        app.world_mut()
            .spawn(Transform::default())
            .insert(PropertyAnimator {
                clip: std::sync::Arc::new(clip),
                time: 0.0,
                speed: 1.0,
                looping: false,
            });
        app
    }

    /// ADR-10131: property warnings are remembered per world, not per process. Two worlds whose
    /// first entity carries the same bad path each warn once (their entities have the same id,
    /// so a process-wide "already warned" set silenced the second world). This is also what made
    /// `perf_g_bad_paths_warn_once` flaky: other tests in the process filled the shared set's cap.
    #[test]
    fn property_warnings_belong_to_their_world() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let path = "nodotPerWorldWarning";
        for world in 0..2 {
            let seen = Arc::new(AtomicUsize::new(0));
            let mut app = app_with_track(path);
            let subscriber = WarnCount {
                seen: Arc::clone(&seen),
                needles: vec![path.to_string()],
            };
            tracing::subscriber::with_default(subscriber, || {
                app.update();
                app.update();
            });
            assert_eq!(
                seen.load(Ordering::Relaxed),
                1,
                "world {world}: one warning for its bad path across two updates"
            );
        }
    }

    #[test]
    fn apply_f32_reports_unsupported_leaf_types() {
        let mut number = 0.0_f32;
        assert!(apply_f32(&mut number, 2.5));
        assert_eq!(number, 2.5);
        let mut flag = false;
        assert!(apply_f32(&mut flag, 1.0));
        assert!(flag);
        let mut text = String::from("keep");
        assert!(!apply_f32(&mut text, 1.0));
        assert_eq!(text, "keep");
    }

    #[test]
    fn integer_leaves_round_and_bool_leaves_are_nonzero() {
        let mut whole = 0_i32;
        assert!(apply_f32(&mut whole, 1.6));
        assert_eq!(whole, 2);
        assert!(apply_f32(&mut whole, -1.6));
        assert_eq!(whole, -2);
        assert!(apply_f32(&mut whole, 1.0e20));
        assert_eq!(whole, i32::MAX);
        assert!(apply_f32(&mut whole, f32::NAN));
        assert_eq!(whole, 0);

        let mut flag = true;
        assert!(apply_f32(&mut flag, 0.0));
        assert!(!flag);
        assert!(apply_f32(&mut flag, 1.0));
        assert!(flag);
        assert!(apply_f32(&mut flag, 0.1));
        assert!(flag);
        assert!(apply_f32(&mut flag, -3.0));
        assert!(flag);
        assert!(apply_f32(&mut flag, f32::NAN));
        assert!(flag);

        let mut other = 7_u32;
        assert!(!apply_f32(&mut other, 1.6));
        assert_eq!(other, 7);
    }

    #[test]
    fn advances_time_and_loops() {
        let mut clip = AnimationClip::empty("door", 2.0);
        clip.property_tracks.push(PropertyTrack {
            path: "Transform.translation.y".into(),
            curve: crate::clip::TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![0.0, 3.0],
                exact_rotation: false,
            },
        });
        let animator = PropertyAnimator {
            clip: std::sync::Arc::new(clip),
            time: 0.0,
            speed: 1.0,
            looping: true,
        };
        // Time advance semantics come from AnimationClip::advance; sanity-check here.
        let (t, wrapped) = AnimationClip::advance(animator.time, 1.5, animator.clip.duration, true);
        assert_eq!(t, 1.5);
        assert!(!wrapped);
        let (t, wrapped) = AnimationClip::advance(t, 1.0, animator.clip.duration, true);
        assert_eq!(t, 0.5);
        assert!(wrapped);
    }

    mod first {
        use regia_ecs::prelude::*;

        /// Shares its short name with `second::Dup`.
        #[derive(Component, Reflect, Default)]
        #[reflect(Component, Default)]
        pub struct Dup {
            pub value: f32,
        }
    }

    mod second {
        use regia_ecs::prelude::*;

        /// Shares its short name with `first::Dup`.
        #[derive(Component, Reflect, Default)]
        #[reflect(Component, Default)]
        pub struct Dup {
            pub value: f32,
        }
    }

    #[test]
    fn a_shared_short_name_resolves_by_full_path_whatever_the_registry_order() {
        // The registry iterates in `TypeId` order, which differs between builds (ADR-867).
        let lookup = |second_first: bool| {
            let mut registry = regia_ecs::reflect::TypeRegistry::default();
            if second_first {
                registry.register::<second::Dup>();
                registry.register::<first::Dup>();
            } else {
                registry.register::<first::Dup>();
                registry.register::<second::Dup>();
            }
            component_name_map(&registry)
                .into_iter()
                .find(|(name, _)| name == "Dup")
                .map(|(_, id)| id)
        };
        let want = Some(std::any::TypeId::of::<first::Dup>());
        assert_eq!(lookup(false), want, "`first::Dup` sorts before `second::Dup`");
        assert_eq!(lookup(true), want);
    }

    /// `ReflectComponent` data added to an already-registered type later (the registration
    /// count does not change) used to leave the short-name cache stale: the type stayed
    /// unresolved and its property tracks never applied.
    #[test]
    fn reflect_data_added_after_registration_is_picked_up() {
        use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};

        /// Registered without `#[reflect(Component)]`: the raw registration carries no
        /// `ReflectComponent` data until `register_type_data` adds it below.
        #[derive(Component, Reflect, Default)]
        #[reflect(Default)]
        struct LateBind {
            level: f32,
        }

        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins);
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_nanos(16_666_667),
        ));
        app.add_systems(regia_app::FixedUpdate, apply_property_animators);
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        registry.write().register::<LateBind>();

        let mut clip = AnimationClip::empty("lamp", 2.0);
        clip.property_tracks.push(PropertyTrack {
            path: "LateBind.level".into(),
            curve: crate::clip::TrackCurve::Static(1.0),
        });
        let entity = app
            .world_mut()
            .spawn((
                LateBind::default(),
                PropertyAnimator {
                    clip: std::sync::Arc::new(clip),
                    time: 0.0,
                    speed: 1.0,
                    looping: false,
                },
            ))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<LateBind>(entity).map(|c| c.level),
            Some(0.0),
            "without ReflectComponent data the track cannot apply yet"
        );

        // No new type registers: the registration count is unchanged. The name cache must
        // still notice the data change and resolve the component on the next update.
        registry
            .write()
            .register_type_data::<LateBind, ReflectComponent>();
        app.update();
        assert_eq!(
            app.world().get::<LateBind>(entity).map(|c| c.level),
            Some(1.0),
            "the track applies once ReflectComponent data exists"
        );
    }

    #[test]
    fn applies_to_registered_transform() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins);
        app.register_type::<Transform>();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_nanos(16_666_667),
        ));
        app.add_systems(regia_app::FixedUpdate, apply_property_animators);

        let mut clip = AnimationClip::empty("door", 2.0);
        clip.property_tracks.push(PropertyTrack {
            path: "Transform.translation.y".into(),
            curve: crate::clip::TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![0.0, 3.0],
                exact_rotation: false,
            },
        });
        let entity = app
            .world_mut()
            .spawn(Transform::default())
            .insert(PropertyAnimator {
                clip: std::sync::Arc::new(clip),
                time: 1.0,
                speed: 1.0,
                looping: false,
            })
            .id();
        app.update();
        let transform = app.world().get::<Transform>(entity).copied();
        let Some(transform) = transform else {
            panic!("transform missing");
        };
        // One fixed step advanced the clock by 1/60: the track value is 3·t/2 at t = 1 + 1/60.
        assert!(
            (transform.translation.y - 1.525).abs() < 1e-4,
            "{}",
            transform.translation.y
        );
    }

    /// Audit perf G finding 3: the second fixed step must sample the door and must not rebuild
    /// the short-name table or clone the track path.
    #[test]
    fn perf_g_second_update_reuses_the_name_map() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins);
        app.register_type::<Transform>();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_nanos(16_666_667),
        ));
        app.add_systems(regia_app::FixedUpdate, apply_property_animators);

        let mut clip = AnimationClip::empty("door", 2.0);
        clip.property_tracks.push(PropertyTrack {
            path: "Transform.translation.y".into(),
            curve: crate::clip::TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![0.0, 3.0],
                exact_rotation: false,
            },
        });
        let entity = app
            .world_mut()
            .spawn(Transform::default())
            .insert(PropertyAnimator {
                clip: std::sync::Arc::new(clip),
                time: 1.0,
                speed: 1.0,
                looping: false,
            })
            .id();
        // The production path must not call the path-clone hook. Naming it keeps the counter.
        let _hook: fn() = super::perf_work::note_path_clone;
        super::perf_work::reset();
        app.update();
        let y1 = app.world().get::<Transform>(entity).map(|t| t.translation.y);
        let maps = super::perf_work::name_maps();
        let clones = super::perf_work::path_clones();
        app.update();
        let y2 = app.world().get::<Transform>(entity).map(|t| t.translation.y);
        let maps_after = super::perf_work::name_maps();
        let clones_after = super::perf_work::path_clones();
        let Some(y1) = y1 else {
            panic!("transform missing after the first step")
        };
        let Some(y2) = y2 else {
            panic!("transform missing after the second step")
        };
        assert!((y1 - 1.525).abs() < 1e-4, "first step y={y1}");
        assert!((y2 - 1.55).abs() < 1e-4, "second step y={y2}");
        assert_eq!(maps, 1, "the first step builds the name map once, built {maps}");
        assert!(
            maps_after == maps && clones_after == clones,
            "second update rebuilt the name map ({maps} -> {maps_after}) or cloned the track path ({clones} -> {clones_after})"
        );
    }

    /// Audit perf G finding 5: a missing component and a path with no dot each warn once.
    #[test]
    fn perf_g_bad_paths_warn_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let missing = format!("Missing{n}.x");
        let nodot = format!("nodot{n}");
        let seen = Arc::new(AtomicUsize::new(0));

        // One app animating a component that is not registered (`missing`) and a path with no
        // dot (`nodot`).
        let app_with = |missing: &str, nodot: &str| {
            let mut app = regia_app::App::new();
            app.add_plugins(regia_app::MinimalPlugins);
            app.register_type::<Transform>();
            app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_nanos(16_666_667),
            ));
            app.add_systems(regia_app::FixedUpdate, apply_property_animators);
            let mut clip = AnimationClip::empty("door", 2.0);
            let curve = crate::clip::TrackCurve::Static(1.0);
            clip.property_tracks.push(PropertyTrack {
                path: missing.to_string(),
                curve: curve.clone(),
            });
            clip.property_tracks.push(PropertyTrack {
                path: nodot.to_string(),
                curve,
            });
            app.world_mut()
                .spawn(Transform::default())
                .insert(PropertyAnimator {
                    clip: std::sync::Arc::new(clip),
                    time: 0.0,
                    speed: 1.0,
                    looping: false,
                });
            app
        };
        let mut app = app_with(&missing, &nodot);
        // Warm-up on other bad paths in another world (their warnings match neither needle):
        // registers both warn callsites before the interest rebuild below. tracing caches one
        // interest per callsite for the process, and a callsite another test thread registers for
        // the first time takes that thread's "never" while only this scoped subscriber is alive
        // (the race of `eval::tests::evaluate_graph_enters_a_span`, ADR-8735; the v0.6.1 fix, kept
        // next to ADR-10131's per-world warn-once set).
        let mut warm = app_with(&format!("Unregistered{n}.x"), &format!("dotless{n}"));
        // The unregistered-component warning names the component (`Missing0`), not the whole
        // track path (`Missing0.x`).
        let missing_component = missing.split('.').next().unwrap_or(&missing).to_string();
        let subscriber = WarnCount {
            seen: Arc::clone(&seen),
            needles: vec![missing_component, nodot],
        };
        tracing::subscriber::with_default(subscriber, || {
            warm.update();
            warm.update();
            tracing::callsite::rebuild_interest_cache();
            app.update();
            app.update();
        });
        assert_eq!(
            seen.load(Ordering::Relaxed),
            2,
            "one warning per bad path across two updates, not one per update"
        );
        // The test owns the counter it checks: the warn-once set is this world's (ADR-10131).
        assert_eq!(app.world().resource::<PropertyWarnings>().len, 2);
    }

    /// ADR-10131: the per-world warn-once set fires once per entity, kind and path, stops at
    /// its cap, and another world's set starts empty.
    #[test]
    fn property_warnings_fire_once_and_stop_at_the_cap() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let mut warnings = PropertyWarnings::default();
        assert!(warnings.first(a, WarnKind::NoDot, "nodot"));
        assert!(!warnings.first(a, WarnKind::NoDot, "nodot"));
        assert!(
            warnings.first(b, WarnKind::NoDot, "nodot"),
            "another entity warns"
        );
        assert!(warnings.first(a, WarnKind::Path, "nodot"), "another kind warns");
        assert!(warnings.first(a, WarnKind::NoDot, "nodot2"), "another path warns");
        while warnings.len < PROPERTY_WARN_CAP {
            let path = format!("fill{}", warnings.len);
            assert!(warnings.first(a, WarnKind::Unknown, &path));
        }
        assert!(
            !warnings.first(b, WarnKind::Missing, "past.cap"),
            "the cap silences new paths"
        );
        assert!(!warnings.first(a, WarnKind::NoDot, "nodot"));
        assert_eq!(warnings.len, PROPERTY_WARN_CAP);
        let mut other = PropertyWarnings::default();
        assert!(
            other.first(b, WarnKind::Missing, "past.cap"),
            "a fresh world starts empty"
        );
    }
}
