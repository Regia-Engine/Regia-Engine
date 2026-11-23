// SPDX-License-Identifier: MPL-2.0
//! The ears follow the active camera (spec 16 § 3), end to end with the real camera component and
//! transform propagation. No device: the headless mixer, fixed frame steps.

use std::time::Duration;

use regia::audio_listener::CameraAudioListenerPlugin;
use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{Audio, AudioListener, AudioListenerFollow, AudioPlugin, AudioSettings};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_render::{Camera, RenderTarget};
use regia_scene::{ScenePlugin, Transform};

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins((ScenePlugin, AudioPlugin, CameraAudioListenerPlugin));
    app
}

fn listeners(app: &App) -> Vec<(Option<Entity>, Vec3, Vec3)> {
    app.world()
        .resource::<Audio>()
        .listeners()
        .iter()
        .map(|(entity, frame)| (*entity, frame.position, frame.forward))
        .collect()
}

fn assert_vec3(actual: Vec3, expected: Vec3) {
    assert!((actual - expected).length() < 1e-5, "{actual} vs {expected}");
}

#[test]
fn the_listener_matches_the_camera_after_one_update() {
    let mut app = app();
    let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    let camera = app
        .world_mut()
        .spawn((
            Camera::default(),
            Transform {
                translation: Vec3::new(3.0, 1.5, -7.0),
                rotation,
                ..Transform::IDENTITY
            },
        ))
        .id();
    app.update();
    let heard = listeners(&app);
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].0, Some(camera));
    assert_vec3(heard[0].1, Vec3::new(3.0, 1.5, -7.0));
    assert_vec3(heard[0].2, rotation * Vec3::NEG_Z);
}

#[test]
fn the_listener_keeps_following_and_takes_its_velocity_from_the_motion() {
    let mut app = app();
    let camera = app
        .world_mut()
        .spawn((Camera::default(), Transform::IDENTITY))
        .id();
    app.update();
    // One metre per 1/60 s frame, for a second: the tracker smooths velocity, so it needs a moment.
    for frame in 1..=60 {
        let at = Vec3::new(frame as f32, 0.0, 0.0);
        app.world_mut()
            .entity_mut(camera)
            .insert(Transform::from_translation(at));
        app.update();
        assert_vec3(listeners(&app)[0].1, at);
    }
    let velocity = app.world().resource::<Audio>().listeners()[0].1.velocity;
    assert!((velocity.x - 60.0).abs() < 1.0, "{velocity}");
}

#[test]
fn a_parented_camera_is_heard_where_it_is_drawn() {
    let mut app = app();
    let rig = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
        .id();
    app.world_mut().spawn((
        Camera::default(),
        Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
        ChildOf(rig),
    ));
    app.update();
    assert_vec3(listeners(&app)[0].1, Vec3::new(10.0, 2.0, 0.0));
}

#[test]
fn an_explicit_listener_takes_over_from_the_camera() {
    let mut app = app();
    app.world_mut().spawn((
        Camera::default(),
        Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
    ));
    let ears = app
        .world_mut()
        .spawn((
            AudioListener::default(),
            Transform::from_translation(Vec3::new(0.0, 9.0, 0.0)),
        ))
        .id();
    app.update();
    let heard = listeners(&app);
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].0, Some(ears));
    assert_vec3(heard[0].1, Vec3::new(0.0, 9.0, 0.0));
}

#[test]
fn the_setting_turns_the_follow_off() {
    let mut app = app();
    app.insert_resource(AudioListenerFollow::Off);
    app.world_mut().spawn((
        Camera::default(),
        Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
    ));
    app.update();
    let heard = listeners(&app);
    assert_vec3(heard.first().map_or(Vec3::ZERO, |l| l.1), Vec3::ZERO);
    assert_eq!(heard.first().and_then(|l| l.0), None);
}

#[test]
fn the_active_camera_is_the_first_window_camera() {
    let mut app = app();
    let camera = |order: i32| Camera {
        order,
        ..Camera::default()
    };
    let texture_target = Camera {
        target: RenderTarget::Texture { asset_id: 1 },
        ..camera(-5)
    };
    app.world_mut().spawn((
        texture_target,
        Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
    ));
    app.world_mut()
        .spawn((camera(2), Transform::from_translation(Vec3::new(2.0, 0.0, 0.0))));
    let chosen = app
        .world_mut()
        .spawn((camera(1), Transform::from_translation(Vec3::new(1.0, 0.0, 0.0))))
        .id();
    app.update();
    let heard = listeners(&app);
    assert_eq!(heard[0].0, Some(chosen));
    assert_vec3(heard[0].1, Vec3::new(1.0, 0.0, 0.0));
}

/// Spec 16 § 3: a camera that does not draw to a window never moves the listener.
#[test]
fn a_texture_only_camera_leaves_the_ears_at_the_origin() {
    let mut app = app();
    app.world_mut().spawn((
        Camera {
            target: RenderTarget::Texture { asset_id: 7 },
            ..Camera::default()
        },
        Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
    ));
    app.update();
    let heard = listeners(&app);
    assert_vec3(heard.first().map_or(Vec3::ZERO, |l| l.1), Vec3::ZERO);
    assert_eq!(heard.first().and_then(|l| l.0), None);
}

/// A window camera whose scale has collapsed still has a place and a facing. Glam's
/// `to_scale_rotation_translation` reciprocates a zero scale into a non-finite rotation
/// (`glam_assert` is off in this workspace), which poisons the listener basis.
#[test]
fn a_degenerate_camera_scale_is_still_heard_at_its_position() {
    for scale in [Vec3::ZERO, Vec3::new(1.0, 0.0, 1.0)] {
        let mut app = app();
        let at = Vec3::new(4.0, 2.0, -1.0);
        let camera = app
            .world_mut()
            .spawn((
                Camera::default(),
                Transform {
                    translation: at,
                    scale,
                    ..Transform::IDENTITY
                },
            ))
            .id();
        app.update();
        let heard = listeners(&app);
        assert_eq!(heard.len(), 1, "scale {scale:?}");
        assert_eq!(heard[0].0, Some(camera), "scale {scale:?}");
        assert_vec3(heard[0].1, at);
        assert!(
            heard[0].2.is_finite(),
            "forward must stay finite for scale {scale:?}: {}",
            heard[0].2
        );
        assert_vec3(heard[0].2, Vec3::NEG_Z);
    }
}

/// A non-finite camera translation must not be published. With no other window camera the ears
/// stay at the origin.
#[test]
fn a_non_finite_camera_translation_does_not_move_the_ears() {
    let mut app = app();
    app.world_mut().spawn((
        Camera::default(),
        Transform {
            translation: Vec3::new(f32::NAN, 1.0, 2.0),
            ..Transform::IDENTITY
        },
    ));
    app.update();
    let heard = listeners(&app);
    assert_vec3(heard.first().map_or(Vec3::ZERO, |l| l.1), Vec3::ZERO);
    assert_eq!(heard.first().and_then(|l| l.0), None);
    assert!(
        heard.first().is_none_or(|l| l.2.is_finite()),
        "forward must stay finite"
    );
}

/// The poisoned window camera is skipped, so a later finite window camera is heard.
#[test]
fn a_non_finite_camera_is_skipped_for_a_finite_window_camera() {
    let mut app = app();
    app.world_mut().spawn((
        Camera {
            order: 0,
            ..Camera::default()
        },
        Transform {
            translation: Vec3::new(f32::NAN, 0.0, 0.0),
            ..Transform::IDENTITY
        },
    ));
    let good = app
        .world_mut()
        .spawn((
            Camera {
                order: 1,
                ..Camera::default()
            },
            Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
        ))
        .id();
    app.update();
    let heard = listeners(&app);
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].0, Some(good));
    assert_vec3(heard[0].1, Vec3::new(2.0, 0.0, 0.0));
    assert!(heard[0].2.is_finite(), "{}", heard[0].2);
}

#[test]
fn removing_the_camera_returns_the_ears_to_the_origin() {
    let mut app = app();
    let camera = app
        .world_mut()
        .spawn((
            Camera::default(),
            Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
        ))
        .id();
    app.update();
    assert_eq!(listeners(&app).len(), 1);
    app.world_mut().despawn(camera);
    app.update();
    let heard = listeners(&app);
    assert_vec3(heard.first().map_or(Vec3::ZERO, |l| l.1), Vec3::ZERO);
    assert_eq!(heard.first().and_then(|l| l.0), None);
}
