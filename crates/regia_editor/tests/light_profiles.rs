// SPDX-License-Identifier: MPL-2.0
//! ADR-2592: `LightProfile`, `RectLight` and `TubeLight` are registered level components, so the editor and agents
//! author them like any other (`editor.set_property`), and the render extraction carries them into the frame's
//! `light_extras`: a profile on the light it names, rect and tube lights as emitters placed by their transforms.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_editor::agent_ext;
use regia_editor::command::{UndoStack, undo_world};
use regia_render::{
    LabLocalLight, LightProfile, LightProfileLibrary, LightSourceLtc, LightSourceSize, PointLight, RectLight,
    SpotLight, TubeLight,
};
use regia_scene::prelude::{EntityGuid, GlobalTransform, Name, Transform};
use serde_json::{Value, json};

fn set(app: &mut App, params: Value) -> Result<Value, String> {
    agent_ext::test_support::set_property(app.world_mut(), &params).map_err(|e| format!("{e:?}"))
}

#[test]
fn set_property_edits_the_rect_light_and_the_profile_and_undoes() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    regia_render::register_render_types(&mut app);
    app.init_resource::<UndoStack>();
    agent_ext::install(app.world_mut());
    let rect = app
        .world_mut()
        .spawn((Name::new("Panel"), Transform::default(), RectLight::default()))
        .id();
    let lamp = app
        .world_mut()
        .spawn((
            Name::new("Lamp"),
            Transform::default(),
            SpotLight::default(),
            LightProfile::default(),
        ))
        .id();
    let out = set(
        &mut app,
        json!({"entity": rect.to_bits(), "component": "RectLight", "path": "width", "value": 2.5}),
    )
    .expect("set width");
    assert_eq!(out["applied"], json!(true));
    assert_eq!(app.world().get::<RectLight>(rect).map(|r| r.width), Some(2.5));
    set(
        &mut app,
        json!({"entity": lamp.to_bits(), "component": "LightProfile", "path": "profile", "value": "lights/dl60.ies"}),
    )
    .expect("set profile");
    assert_eq!(
        app.world().get::<LightProfile>(lamp).map(|p| p.profile.clone()),
        Some("lights/dl60.ies".to_string())
    );
    assert!(
        set(
            &mut app,
            json!({"entity": rect.to_bits(), "component": "RectLight", "path": "two_sided", "value": 3}),
        )
        .is_err()
    );
    assert!(undo_world(app.world_mut()));
    assert!(undo_world(app.world_mut()));
    assert_eq!(app.world().get::<RectLight>(rect).map(|r| r.width), Some(1.0));
}

#[test]
fn the_extraction_carries_profiles_and_area_lights() {
    let mut world = World::new();
    world.init_resource::<regia_render::runtime::ExtractedFrame>();
    let mut library = LightProfileLibrary::default();
    library
        .insert_ies_text("dl60", &regia_asset::ies::example_text())
        .expect("cook");
    world.insert_resource(library);
    let at = |x: f32| {
        let t = Transform::from_translation(regia_core::math::Vec3::new(x, 2.0, 0.0));
        (t, GlobalTransform(t.compute_affine()))
    };
    let (t, g) = at(0.0);
    world.spawn((t, g, PointLight::default()));
    let (t, g) = at(3.0);
    world.spawn((
        t,
        g,
        SpotLight::default(),
        LightProfile {
            profile: "dl60".into(),
            ..LightProfile::default()
        },
    ));
    let (t, g) = at(6.0);
    world.spawn((
        t,
        g,
        PointLight::default(),
        LightProfile {
            profile: "missing".into(),
            ..LightProfile::default()
        },
    ));
    let (t, g) = at(9.0);
    world.spawn((t, g, RectLight::default()));
    let (t, g) = at(12.0);
    world.spawn((t, g, TubeLight::default()));
    world
        .run_system_once(regia_render::extract_frame)
        .expect("extract");
    let frame = world.resource::<regia_render::runtime::ExtractedFrame>();
    let extras = &frame.light_extras;
    assert_eq!(
        extras.profiles.len(),
        1,
        "one profile in use; the missing name is skipped"
    );
    let profiled: Vec<usize> = extras
        .per_light
        .iter()
        .enumerate()
        .filter_map(|(i, u)| u.map(|_| i))
        .collect();
    assert_eq!(profiled.len(), 1);
    // The profiled light is the spot (points are extracted before spots).
    assert!(matches!(
        frame.local_lights[profiled[0]],
        regia_render::LabLocalLight::Spot(_)
    ));
    let mut centres: Vec<f32> = extras.area_lights.iter().map(|a| a.light.centre[0]).collect();
    centres.sort_by(f32::total_cmp);
    assert_eq!(centres, vec![9.0, 12.0]);
}

#[test]
fn the_extraction_marks_ltc_spheres_on_the_light_that_has_the_marker() {
    let mut world = World::new();
    world.init_resource::<regia_render::runtime::ExtractedFrame>();
    let at = |x: f32| {
        let t = Transform::from_translation(regia_core::math::Vec3::new(x, 2.0, 0.0));
        (t, GlobalTransform(t.compute_affine()))
    };
    let (t, g) = at(0.0);
    world.spawn((t, g, PointLight::default(), LightSourceSize { radius_m: 0.2 }));
    let (t, g) = at(3.0);
    world.spawn((
        t,
        g,
        PointLight::default(),
        LightSourceSize { radius_m: 0.2 },
        LightSourceLtc,
    ));
    let (t, g) = at(6.0);
    world.spawn((
        t,
        g,
        SpotLight::default(),
        LightSourceSize { radius_m: 0.1 },
        LightSourceLtc,
    ));
    world
        .run_system_once(regia_render::extract_frame)
        .expect("extract");
    let frame = world.resource::<regia_render::runtime::ExtractedFrame>();
    let mut marked: Vec<(f32, bool)> = frame
        .local_lights
        .iter()
        .enumerate()
        .map(|(i, light)| {
            let x = match light {
                LabLocalLight::Point(p) => p.position[0],
                LabLocalLight::Spot(s) => s.position[0],
            };
            (x, frame.light_extras.ltc_spheres.get(i).copied().unwrap_or(false))
        })
        .collect();
    marked.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(marked, vec![(0.0, false), (3.0, true), (6.0, true)]);

    // No marker in the world: no extras at all, so the frame takes the path it took before.
    let mut plain = World::new();
    plain.init_resource::<regia_render::runtime::ExtractedFrame>();
    let (t, g) = at(0.0);
    plain.spawn((t, g, PointLight::default(), LightSourceSize { radius_m: 0.2 }));
    plain
        .run_system_once(regia_render::extract_frame)
        .expect("extract");
    assert!(
        plain
            .resource::<regia_render::runtime::ExtractedFrame>()
            .light_extras
            .is_empty()
    );
}

#[test]
fn the_marker_is_a_registered_component_agents_can_add() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    regia_render::register_render_types(&mut app);
    let registry = app
        .world()
        .resource::<regia_ecs::reflect::AppTypeRegistry>()
        .read();
    let registration = registry
        .get_with_short_type_path("LightSourceLtc")
        .expect("LightSourceLtc is registered");
    assert!(
        registration
            .data::<regia_ecs::reflect::ReflectComponent>()
            .is_some()
    );
}

#[test]
fn the_light_components_survive_a_scene_round_trip() {
    let build = || {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        regia_render::register_render_types(&mut app);
        app
    };
    let rect = RectLight {
        width: 2.5,
        height: 0.75,
        two_sided: true,
        ..RectLight::default()
    };
    let tube = TubeLight {
        length: 1.5,
        radius: 0.04,
        ..TubeLight::default()
    };
    let profile = LightProfile {
        profile: "lights/dl60.ies".into(),
        rotation: 0.5,
        ..LightProfile::default()
    };
    let mut saved = build();
    let world = saved.world_mut();
    world.spawn((
        EntityGuid(Guid::new_random()),
        Name::new("Panel"),
        Transform::default(),
        rect.clone(),
    ));
    world.spawn((
        EntityGuid(Guid::new_random()),
        Name::new("Strip"),
        Transform::default(),
        tube.clone(),
    ));
    world.spawn((
        EntityGuid(Guid::new_random()),
        Name::new("Lamp"),
        Transform::default(),
        PointLight::default(),
        LightSourceSize { radius_m: 0.2 },
        LightSourceLtc,
        profile.clone(),
    ));
    let file = regia_scene::prelude::snapshot_scene(world, &Default::default()).expect("snapshot");
    let text = file.to_ron_string().expect("ron");
    for name in ["RectLight", "TubeLight", "LightProfile", "LightSourceLtc"] {
        assert!(text.contains(name), "the scene stores {name}:\n{text}");
    }

    let mut loaded = build();
    let parsed = regia_scene::prelude::SceneFile::from_ron_str(&text).expect("parse");
    regia_scene::prelude::spawn_scene(loaded.world_mut(), &parsed, &Default::default()).expect("spawn");
    let world = loaded.world_mut();
    let rects: Vec<RectLight> = world.query::<&RectLight>().iter(world).cloned().collect();
    assert_eq!(rects, vec![rect]);
    let tubes: Vec<TubeLight> = world.query::<&TubeLight>().iter(world).cloned().collect();
    assert_eq!(tubes, vec![tube]);
    let lamps: Vec<LightProfile> = world
        .query_filtered::<&LightProfile, With<LightSourceLtc>>()
        .iter(world)
        .cloned()
        .collect();
    assert_eq!(lamps, vec![profile]);
}
