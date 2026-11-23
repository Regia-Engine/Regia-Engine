// SPDX-License-Identifier: MPL-2.0
//! Spectator modes through the real dispatcher and the frame plugin, without a GPU or sleeps.

use regia_agent::{AgentConfig, AgentPermissions, AgentPlugin, AgentSettings, LocalReply, call_local};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::spectator::{SpectatorCamera, SpectatorDrive, SpectatorPlugin};
use regia_render::Camera;
use regia_scene::Transform;
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, None) {
        LocalReply::Done(result) => result.unwrap_or_else(|error| panic!("{method}: {error:?}")),
        LocalReply::Pending(replies) => {
            for _ in 0..8 {
                app.update();
                if let Ok(reply) = replies.try_recv() {
                    assert!(reply.error.is_none(), "{method}: {:?}", reply.error);
                    return reply.result.expect("result");
                }
            }
            panic!("{method} did not answer within eight deterministic frames");
        }
    }
}

#[test]
fn spectator_modes_dispatch_bind_and_expect_the_render_camera_while_paused() {
    let project = regia_test_utils::unique_temp_dir("spectator_modes_dispatch");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins((AgentPlugin, SpectatorPlugin))
        .register_type::<Transform>()
        .register_type::<Name>();
    enable_fixed_step(&mut app, 60.0);
    app.finish_plugins();
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            rate_limit_per_second: u32::MAX,
            ..Default::default()
        },
        source: None,
    });
    let view = app
        .world_mut()
        .spawn((Name::new("Observer"), Camera::default(), Transform::IDENTITY))
        .id();
    let target = app
        .world_mut()
        .spawn((
            Name::new("Subject"),
            Transform {
                translation: Vec3::new(4.0, 5.0, 6.0),
                rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                ..Transform::IDENTITY
            },
        ))
        .id();
    app.update();
    call(&mut app, "play.pause", json!({}));
    call(
        &mut app,
        "spectator.set",
        json!({ "mode": "pov", "pov_offset": [0, 1, 0] }),
    );
    let bound = call(
        &mut app,
        "spectator.bind",
        json!({ "camera": view.to_bits(), "follow": target.to_bits() }),
    );
    assert_eq!(bound["mode"], "pov", "binding preserves the selected mode");
    app.update();
    let ready = call(
        &mut app,
        "play.wait_until",
        json!({ "condition": {
        "entity": view.to_bits(), "component": "Transform", "path": "translation.y", "op": "eq", "value": 5.0
    } }),
    );
    assert_eq!(ready["satisfied"], true, "{ready}");
    let expected = call(
        &mut app,
        "play.expect",
        json!({ "assertions": [
        { "name": "eye x", "condition": { "entity": view.to_bits(), "component": "Transform", "path": "translation.x", "op": "eq", "value": 3.0 } },
        { "name": "eye z", "condition": { "entity": view.to_bits(), "component": "Transform", "path": "translation.z", "op": "eq", "value": 6.0 } }
    ] }),
    );
    assert_eq!(expected["passed"], 2, "{expected}");
    assert!(app.world().resource::<Time>().paused);
    let pose = app.world().resource::<SpectatorCamera>().pose();
    assert!(
        (pose.1 * Vec3::Y - Vec3::NEG_X).length() < 1e-5,
        "POV preserves roll"
    );

    call(&mut app, "spectator.set", json!({ "mode": "orbit" }));
    let stepped = call(
        &mut app,
        "spectator.step",
        json!({ "look": [0.3, -0.2], "zoom": 2.0 }),
    );
    assert_eq!(stepped["mode"], "orbit");
    app.update();
    let cam = app.world().resource::<SpectatorCamera>();
    let direction = (cam.target().expect("bound target") - cam.position()).normalize();
    assert!((cam.forward() - direction).length() < 1e-5);
    assert_eq!(
        app.world().get::<Transform>(view).expect("view").translation,
        cam.position()
    );

    call(
        &mut app,
        "spectator.set",
        json!({
            "mode": "overhead", "overhead_focus": [10, 2, 20],
            "overhead": { "height": 12, "min_height": 2, "max_height": 30, "pan_speed": 0.5 }
        }),
    );
    let stepped = call(
        &mut app,
        "spectator.step",
        json!({ "frames": 4, "dt": 0.25, "movement": [1, 0, 0], "zoom": 2 }),
    );
    assert_eq!(stepped["overhead_height"], 14.0);
    assert_eq!(stepped["overhead_focus"], json!([17.0, 2.0, 20.0]));
    app.update();
    let cam = app.world().resource::<SpectatorCamera>();
    assert!((cam.forward() - Vec3::NEG_Y).length() < 1e-5);
    assert_eq!(cam.position(), Vec3::new(17.0, 16.0, 20.0));
    assert_eq!(
        app.world().get::<Transform>(view).expect("view").translation,
        cam.position()
    );

    call(&mut app, "spectator.set", json!({ "mode": "pov" }));
    app.update();
    let last = app.world().resource::<SpectatorCamera>().pose();
    app.world_mut().despawn(target);
    app.update();
    assert_eq!(app.world().resource::<SpectatorCamera>().pose(), last);
    assert_eq!(app.world().resource::<SpectatorDrive>().lost, Some(target));
    let state = call(&mut app, "spectator.get", json!({}));
    assert_eq!(state["mode"], "pov");
    drop(app);
    std::fs::remove_dir_all(project).expect("remove test project");
}
