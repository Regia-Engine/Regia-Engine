// SPDX-License-Identifier: MPL-2.0
//! The audio sync's side of the listener follow (spec 16 § 3), with no renderer: the ears take the
//! pose in `AudioActiveCamera` unless a listener component or the setting says otherwise. The
//! camera component end of it is in `crates/regia/tests/audio_listener.rs`.

use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioActiveCamera, AudioCameraPose, AudioEmitter, AudioListener, AudioListenerFollow, AudioPlugin,
    AudioSettings, SoundAsset, SoundAssets, SoundClip,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.insert_resource(AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    app.add_plugins(AudioPlugin);
    app
}

fn pose(app: &mut App, position: Vec3) -> Entity {
    let entity = app.world_mut().spawn_empty().id();
    app.insert_resource(AudioActiveCamera(Some(AudioCameraPose {
        entity,
        position,
        rotation: Quat::IDENTITY,
    })));
    entity
}

fn first_listener(app: &App) -> Option<(Option<Entity>, Vec3)> {
    app.world()
        .resource::<Audio>()
        .listeners()
        .first()
        .map(|(entity, frame)| (*entity, frame.position))
}

#[test]
fn follow_is_the_default_and_the_ears_take_the_camera_pose() {
    let mut app = app();
    assert_eq!(
        *app.world().resource::<AudioListenerFollow>(),
        AudioListenerFollow::ActiveCamera
    );
    let at = Vec3::new(1.0, 2.0, 3.0);
    let camera = pose(&mut app, at);
    app.update();
    assert_eq!(first_listener(&app), Some((Some(camera), at)));
}

#[test]
fn a_listener_component_wins_over_the_camera() {
    let mut app = app();
    pose(&mut app, Vec3::new(1.0, 2.0, 3.0));
    let ears = app
        .world_mut()
        .spawn((AudioListener::default(), Transform::from_translation(Vec3::X)))
        .id();
    app.update();
    assert_eq!(first_listener(&app), Some((Some(ears), Vec3::X)));
}

#[test]
fn the_setting_keeps_the_ears_at_the_origin() {
    let mut app = app();
    app.insert_resource(AudioListenerFollow::Off);
    pose(&mut app, Vec3::new(1.0, 2.0, 3.0));
    app.update();
    let (entity, position) = first_listener(&app).unwrap_or((None, Vec3::ZERO));
    assert_eq!(entity, None);
    assert_eq!(position, Vec3::ZERO);
}

#[test]
fn no_camera_and_no_listener_is_the_origin_as_before() {
    let mut app = app();
    app.update();
    let (entity, position) = first_listener(&app).unwrap_or((None, Vec3::ZERO));
    assert_eq!(entity, None);
    assert_eq!(position, Vec3::ZERO);
}

/// The output level of a loud emitter far from the origin and right next to the camera. It is in
/// range only if the ears moved.
fn level_with(follow: AudioListenerFollow) -> f32 {
    let mut app = app();
    app.insert_resource(follow);
    let clip = SoundClip::new(vec![0.25; 48_000], 48_000).expect("clip");
    app.world_mut()
        .resource_mut::<SoundAssets>()
        .add("hum", SoundAsset::from_clip(&clip));
    let far = Vec3::new(5_000.0, 0.0, 0.0);
    pose(&mut app, far + Vec3::Z);
    app.world_mut().spawn((
        AudioEmitter {
            sound_name: "hum".into(),
            max_distance: 10.0,
            occlusion: false,
            ..AudioEmitter::default()
        },
        Transform::from_translation(far),
    ));
    let mut peak = 0.0f32;
    for _ in 0..10 {
        app.update();
        let mut audio = app.world_mut().resource_mut::<Audio>();
        let block = audio.render_offline(800).expect("the offline backend");
        peak = peak.max(block.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs())));
    }
    peak
}

#[test]
fn an_emitter_is_heard_from_the_camera() {
    assert!(level_with(AudioListenerFollow::ActiveCamera) > 0.05);
    assert!(level_with(AudioListenerFollow::Off) < 1e-3, "5 km from the ears");
}
