// SPDX-License-Identifier: MPL-2.0
//! `render.lights`: the directional lights and ambient volumes the frame draws (book/agents/
//! render-lights.md, ADR-7211).

use regia_app::prelude::*;
use regia_render::ExtractedFrame;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn direction_row(l: &regia_render::DirectionalLightUniform) -> Value {
    json!({
        "direction": l.direction,
        "color": l.color,
        "illuminance": l.illuminance,
    })
}

/// `render.lights`: the sun (with its cascaded shadow map), the extra directional lights, the
/// ambient volumes, what the last extraction refused past the caps, and the forward shader's
/// debug view code.
pub(crate) fn lights(world: &mut World) -> Result<Value, RpcError> {
    let frame = world.get_resource::<ExtractedFrame>().ok_or_else(|| {
        RpcError::not_found("no extracted frame in this app", "add regia_render::RenderPlugin")
    })?;
    let views = &frame.lights_views;
    let sun = frame.directional.as_ref().map(|l| {
        let mut row = direction_row(l);
        row["shadows"] = json!(frame.directional_shadows);
        row
    });
    let extra: Vec<Value> = views.extra_directional.iter().map(direction_row).collect();
    let volumes: Vec<Value> = views
        .ambient_volumes
        .iter()
        .map(|v| {
            json!({
                "center": v.center,
                "half_extents": v.half_extents,
                "irradiance": v.irradiance,
                "blend_distance": v.blend_distance,
                "rotation": v.rotation,
            })
        })
        .collect();
    Ok(json!({
        "sun": sun,
        "extra_directional": extra,
        "refused_directional": views.refused_directional,
        "max_directional": regia_render::MAX_DIRECTIONAL_LIGHTS,
        "ambient_volumes": volumes,
        "refused_ambient_volumes": views.refused_ambient_volumes,
        "max_ambient_volumes": regia_render::MAX_AMBIENT_VOLUMES,
        "debug_view_code": regia_render::lights_views::shading_view_code(views.shading_view),
        "extra_directional_paths": regia_render::lights_views::EXTRA_DIRECTIONAL_CONSUMERS,
        "sun_only_paths": regia_render::lights_views::SUN_ONLY_PATHS,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lights_reports_the_sun_the_extras_and_the_refusals() {
        let mut world = World::new();
        assert!(lights(&mut world).is_err(), "no frame: F0003");
        let mut frame = ExtractedFrame {
            directional: Some(regia_render::DirectionalLightUniform::new(
                [0.0, -1.0, 0.0],
                [1.0; 3],
                1e5,
            )),
            directional_shadows: true,
            ..ExtractedFrame::default()
        };
        frame.lights_views.extra_directional =
            vec![regia_render::DirectionalLightUniform::new([1.0, 0.0, 0.0], [1.0; 3], 10.0); 3];
        frame.lights_views.refused_directional = 2;
        frame.lights_views.ambient_volumes = vec![regia_render::FrameAmbientVolume::default()];
        frame.lights_views.shading_view = regia_render::DebugViewMode::Normals;
        world.insert_resource(frame);
        let got = lights(&mut world).expect("lights");
        assert_eq!(got["sun"]["shadows"], json!(true));
        assert_eq!(got["extra_directional"].as_array().map(Vec::len), Some(3));
        assert_eq!(got["refused_directional"], json!(2));
        assert_eq!(got["max_directional"], json!(4));
        assert_eq!(got["ambient_volumes"].as_array().map(Vec::len), Some(1));
        assert_eq!(got["debug_view_code"], json!(2));
        // Which paths shade the extra lights (ADR-7254).
        let paths = got["extra_directional_paths"].as_array().expect("paths");
        for name in [
            "forward",
            "terrain",
            "grass",
            "ddgi_probes",
            "lightmap_bake_cpu",
            "path_tracer",
            // A `.bake.ron` lists its suns since format v3 (ADR-7818).
            "bake_description",
        ] {
            assert!(paths.iter().any(|p| p == name), "{name} is listed");
        }
        let sun_only = got["sun_only_paths"].as_array().expect("sun-only paths");
        for name in [
            "scatter_impostors",
            "water",
            "level_bake_export",
            "editor_bake_panel",
        ] {
            assert!(sun_only.iter().any(|v| v == name), "{name} sees only the sun");
        }
        assert!(
            !sun_only.iter().any(|v| v == "bake_description"),
            "a bake description takes every sun: {sun_only:?}"
        );
    }

    /// Through the dispatcher: `render.lights` reads what extraction picked from five lights and
    /// `render.get_debug_view` reports a new mode the setter took.
    #[test]
    fn render_lights_through_the_dispatcher() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>()
            .insert_resource(regia_render::DebugView::default())
            .add_systems(Render, regia_render::runtime::extract_frame);
        let mut spawned: Vec<(u64, f32)> = [100_000.0, 1.0, 2.0, 3.0, 4.0]
            .into_iter()
            .map(|lux| {
                let entity = app.world_mut().spawn(regia_render::DirectionalLight {
                    direction: [0.0, -1.0, 0.0],
                    color: [1.0; 3],
                    illuminance: lux,
                    cast_shadows: lux > 10.0,
                    static_shadow: false,
                });
                (entity.id().to_bits(), lux)
            })
            .collect();
        spawned.sort_by_key(|(bits, _)| *bits);
        app.update();
        let world = app.world_mut();
        world.insert_resource(crate::safety::AgentPermissions {
            config: crate::safety::AgentConfig::fail_closed(),
            source: None,
        });
        let got = crate::dispatch::call_in_process(world, "render.lights", json!({}))
            .expect("synchronous method")
            .expect("lights");
        // The sun is the lowest-entity light.
        assert_eq!(got["sun"]["illuminance"], json!(spawned[0].1));
        assert_eq!(got["extra_directional"].as_array().map(Vec::len), Some(3));
        assert_eq!(got["refused_directional"], json!(1));
        assert!(
            got["extra_directional_paths"]
                .as_array()
                .is_some_and(|p| p.iter().any(|v| v == "grass")),
            "{got}"
        );
        assert!(
            got["extra_directional_paths"]
                .as_array()
                .is_some_and(|p| p.iter().any(|v| v == "bake_description")),
            "a .bake.ron v3 bakes every sun (ADR-7818): {got}"
        );
        // The setter itself (the fail-closed permissions above refuse edits through the dispatcher).
        let set = crate::studio_lighting::set_debug_view(world, &json!({ "mode": "light_complexity" }))
            .expect("a new mode name parses");
        assert_eq!(set["mode"], json!("light_complexity"));
        let modes = crate::dispatch::call_in_process(world, "render.get_debug_view", json!({}))
            .expect("synchronous method")
            .expect("modes");
        assert_eq!(modes["mode"], json!("light_complexity"));
        // post05w4: a turned AmbientVolume reports its rotation and bounds along its own axes.
        let turn = regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_4);
        world.spawn((
            regia_render::AmbientVolume::default(),
            regia_scene::GlobalTransform(regia_core::math::Affine3A::from_rotation_translation(
                turn,
                regia_core::math::Vec3::ZERO,
            )),
        ));
        app.update();
        let world = app.world_mut();
        let got = crate::dispatch::call_in_process(world, "render.lights", json!({}))
            .expect("synchronous method")
            .expect("lights");
        let rotation = &got["ambient_volumes"][0]["rotation"];
        for (i, want) in turn.to_array().into_iter().enumerate() {
            let have = rotation[i].as_f64().unwrap_or(f64::NAN);
            assert!((have - f64::from(want)).abs() < 1e-6, "rotation {rotation}");
        }
        for name in ["unlit", "overdraw", "lod_color", "mip_level"] {
            assert!(
                modes["modes"]
                    .as_array()
                    .is_some_and(|m| m.iter().any(|v| v == name)),
                "{name} is listed"
            );
        }
    }
}
