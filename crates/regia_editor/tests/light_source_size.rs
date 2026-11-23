// SPDX-License-Identifier: MPL-2.0
//! ADR-1367: `LightSourceSize` is a registered level component, so the editor and agents author
//! it like any other: `editor.set_property` edits `radius_m` as one undo step, and the render
//! extraction carries the edited radius into the frame's lab light.

use regia_app::prelude::*;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_editor::agent_ext;
use regia_editor::command::{UndoStack, undo_world};
use regia_render::{LabLocalLight, LightSourceSize, PointLight, SpotLight};
use regia_scene::prelude::{GlobalTransform, Name, Transform};
use serde_json::{Value, json};

fn app_with_light() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    regia_render::register_render_types(&mut app);
    app.init_resource::<UndoStack>();
    agent_ext::install(app.world_mut());
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Bulb"),
            Transform::default(),
            PointLight::default(),
            LightSourceSize::default(),
        ))
        .id();
    (app, entity)
}

fn set(app: &mut App, params: Value) -> Result<Value, String> {
    agent_ext::test_support::set_property(app.world_mut(), &params).map_err(|e| format!("{e:?}"))
}

fn radius(app: &App, entity: Entity) -> f32 {
    app.world()
        .get::<LightSourceSize>(entity)
        .map(|s| s.radius_m)
        .expect("source size")
}

#[test]
fn set_property_edits_the_source_radius_and_undoes() {
    let (mut app, entity) = app_with_light();
    let id = entity.to_bits();
    assert_eq!(
        radius(&app, entity),
        regia_render::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M
    );
    let result = set(
        &mut app,
        json!({"entity": id, "component": "LightSourceSize", "path": "radius_m", "value": 0.2}),
    )
    .expect("set radius");
    assert_eq!(result["applied"], json!(true));
    assert_eq!(result["after"].as_f64().map(|v| v as f32), Some(0.2));
    assert_eq!(radius(&app, entity), 0.2);
    // A dry run reports the value without changing it.
    set(
        &mut app,
        json!({"entity": id, "component": "LightSourceSize", "path": "radius_m", "value": 1.0, "dry_run": true}),
    )
    .expect("dry run");
    assert_eq!(radius(&app, entity), 0.2);
    // A string is not a radius.
    assert!(
        set(
            &mut app,
            json!({"entity": id, "component": "LightSourceSize", "path": "radius_m", "value": "big"}),
        )
        .is_err()
    );
    assert!(undo_world(app.world_mut()));
    assert_eq!(
        radius(&app, entity),
        regia_render::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M
    );
}

#[test]
fn the_extraction_carries_the_source_radius_into_the_lab_lights() {
    let mut world = World::new();
    world.init_resource::<regia_render::runtime::ExtractedFrame>();
    let at = |x: f32| {
        let t = Transform::from_translation(regia_core::math::Vec3::new(x, 2.0, 0.0));
        (t, GlobalTransform(t.compute_affine()))
    };
    let (t, g) = at(0.0);
    world.spawn((t, g, PointLight::default(), LightSourceSize { radius_m: 0.2 }));
    let (t, g) = at(3.0);
    world.spawn((t, g, PointLight::default()));
    let (t, g) = at(6.0);
    world.spawn((t, g, SpotLight::default(), LightSourceSize { radius_m: -1.0 }));
    let (t, g) = at(9.0);
    world.spawn((
        t,
        g,
        PointLight::default(),
        LightSourceSize { radius_m: f32::NAN },
    ));
    world
        .run_system_once(regia_render::extract_frame)
        .expect("extract");
    let frame = world.resource::<regia_render::runtime::ExtractedFrame>();
    let mut radii: Vec<(f32, Option<f32>)> = frame
        .local_lights
        .iter()
        .map(|light| match light {
            LabLocalLight::Point(p) => (p.position[0], p.source_radius),
            LabLocalLight::Spot(s) => (s.position[0], s.source_radius),
        })
        .collect();
    radii.sort_by(|a, b| a.0.total_cmp(&b.0));
    // A negative radius is a true point (0); a non-finite one counts as absent.
    assert_eq!(
        radii,
        vec![(0.0, Some(0.2)), (3.0, None), (6.0, Some(0.0)), (9.0, None)]
    );
}
