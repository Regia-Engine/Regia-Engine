// SPDX-License-Identifier: MPL-2.0
//! Spec 52 § 3 (deferred by ADR-1831, built by ADR-7216): the motion-matching component, its
//! settings and its player derive `Reflect`, and `MotionMatchPlugin` registers them, so an
//! inspector or an agent reads and patches their tuning fields by path. The shared database, the
//! clips and the skeleton (assets behind `Arc`) and the runtime state stay unreflected.

use std::any::TypeId;
use std::sync::Arc;

use regia_animation::motion_matching::procedural::procedural_locomotion;
use regia_animation::motion_matching::{
    MatcherSettings, MotionMatchPlayer, MotionMatchPlugin, MotionMatching, PlayerWarps, SearchMethod,
    TrajectoryPoint, WarpInputs,
};
use regia_animation::prelude::*;
use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_ecs::prelude::ReflectDefault;
use regia_ecs::reflect::{GetPath, PartialReflect, ReflectComponent};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin, MotionMatchPlugin));
    app
}

fn components() -> (MotionMatching, MotionMatchPlayer) {
    let set = procedural_locomotion();
    let database = Arc::new(set.build().expect("procedural database builds"));
    let matching = MotionMatching::new(database, MatcherSettings::default());
    let clips: Arc<[AnimationClip]> = set.entry_clips().into();
    let player = MotionMatchPlayer::new(clips, Arc::new(set.skeleton.clone()));
    (matching, player)
}

#[test]
fn the_plugin_registers_the_three_types_for_reflection() {
    let app = app();
    let registry = app.world().resource::<AppTypeRegistry>().read();
    for (name, id) in [
        ("MotionMatching", TypeId::of::<MotionMatching>()),
        ("MotionMatchPlayer", TypeId::of::<MotionMatchPlayer>()),
    ] {
        let registration = registry
            .get(id)
            .unwrap_or_else(|| panic!("{name} is not registered"));
        assert!(
            registration.data::<ReflectComponent>().is_some(),
            "{name} is not a reflected component"
        );
    }
    let settings = registry
        .get(TypeId::of::<MatcherSettings>())
        .expect("MatcherSettings is registered");
    assert!(
        settings.data::<ReflectDefault>().is_some(),
        "MatcherSettings reflects Default"
    );
    for id in [
        TypeId::of::<SearchMethod>(),
        TypeId::of::<TrajectoryPoint>(),
        TypeId::of::<PlayerWarps>(),
        TypeId::of::<WarpInputs>(),
    ] {
        assert!(
            registry.get(id).is_some(),
            "a field type is not registered: {id:?}"
        );
    }
}

#[test]
fn settings_fields_read_and_patch_by_path() {
    let mut settings = MatcherSettings::default();
    let rate = settings
        .reflect_path_mut("play_rate")
        .ok()
        .and_then(|f| f.try_downcast_mut::<f32>())
        .expect("play_rate is a reflected f32");
    *rate = 1.5;
    let method = settings.reflect_path("method").expect("method is reflected");
    let PartialReflectEnum(variant) = enum_variant(method);
    assert_eq!(variant, "Auto");
    assert!(
        settings
            .reflect_path("urgent_threshold")
            .expect("urgent_threshold is reflected")
            .reflect_partial_eq(&None::<f32>)
            .unwrap_or(false)
    );
    assert_eq!(settings.play_rate, 1.5);
    assert_eq!(MatcherSettings::default().play_rate, 1.0);
}

/// The variant name of a reflected enum field.
struct PartialReflectEnum(String);

fn enum_variant(field: &dyn PartialReflect) -> PartialReflectEnum {
    match field.reflect_ref() {
        regia_ecs::reflect::ReflectRef::Enum(e) => PartialReflectEnum(e.variant_name().to_owned()),
        other => panic!("not an enum: {:?}", other.kind()),
    }
}

#[test]
fn a_spawned_matcher_and_player_are_patched_through_reflect_component() {
    let mut app = app();
    let (matching, player) = components();
    let rig = app.world_mut().spawn((matching, player)).id();
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let reflect_matching = registry
        .get(TypeId::of::<MotionMatching>())
        .and_then(|r| r.data::<ReflectComponent>())
        .expect("MotionMatching reflects Component")
        .clone();
    let reflect_player = registry
        .get(TypeId::of::<MotionMatchPlayer>())
        .and_then(|r| r.data::<ReflectComponent>())
        .expect("MotionMatchPlayer reflects Component")
        .clone();
    {
        let mut entity = app.world_mut().entity_mut(rig);
        let value = reflect_matching
            .reflect_mut(&mut entity)
            .expect("the rig has MotionMatching")
            .into_inner();
        *value
            .reflect_path_mut("report_candidates")
            .ok()
            .and_then(|f| f.try_downcast_mut::<usize>())
            .expect("report_candidates") = 4;
        *value
            .reflect_path_mut("matcher.settings.search_interval")
            .ok()
            .and_then(|f| f.try_downcast_mut::<f32>())
            .expect("matcher.settings.search_interval") = 0.25;
        *value
            .reflect_path_mut("trajectory[0].position")
            .ok()
            .and_then(|f| f.try_downcast_mut::<Vec2>())
            .expect("trajectory[0].position") = Vec2::new(0.5, 1.0);
    }
    {
        let mut entity = app.world_mut().entity_mut(rig);
        let value = reflect_player
            .reflect_mut(&mut entity)
            .expect("the rig has MotionMatchPlayer")
            .into_inner();
        *value
            .reflect_path_mut("duration_max")
            .ok()
            .and_then(|f| f.try_downcast_mut::<f32>())
            .expect("duration_max") = 0.4;
        *value
            .reflect_path_mut("warp_inputs.actual_speed")
            .ok()
            .and_then(|f| f.try_downcast_mut::<Option<f32>>())
            .expect("warp_inputs.actual_speed") = Some(2.0);
        *value
            .reflect_path_mut("warps.stride.strength")
            .ok()
            .and_then(|f| f.try_downcast_mut::<f32>())
            .expect("warps.stride.strength") = 0.5;
    }
    let matching = app.world().get::<MotionMatching>(rig).expect("matching");
    assert_eq!(matching.report_candidates, 4);
    assert_eq!(matching.matcher.settings.search_interval, 0.25);
    assert_eq!(matching.trajectory[0].position, Vec2::new(0.5, 1.0));
    let player = app.world().get::<MotionMatchPlayer>(rig).expect("player");
    assert_eq!(player.duration_max, 0.4);
    assert_eq!(player.warp_inputs.actual_speed, Some(2.0));
    assert_eq!(player.warps.stride.strength, 0.5);
    // The shared database and the assets are not reflected fields.
    let entity = app.world().entity(rig);
    let reflected = reflect_matching.reflect(entity).expect("reflect");
    assert!(reflected.reflect_path("database").is_err());
    let reflected = reflect_player.reflect(entity).expect("reflect");
    assert!(reflected.reflect_path("clips").is_err());
    assert!(reflected.reflect_path("skeleton").is_err());
    // Neither can be rebuilt from reflection (an `Arc` asset has no default), so a reflected
    // clone (what a scene snapshot takes) is refused rather than half-copied: saves skip them.
    assert!(
        reflected.reflect_clone().is_err(),
        "the player clones through reflection"
    );
    let reflected = reflect_matching.reflect(entity).expect("reflect");
    assert!(
        reflected.reflect_clone().is_err(),
        "the matcher clones through reflection"
    );
}
