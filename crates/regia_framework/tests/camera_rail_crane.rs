// SPDX-License-Identifier: MPL-2.0
//! Rail and crane cameras end to end (ADR-3231): a sequence keys a dolly move and a crane swing
//! through the property appliers, and the driver puts the camera at the exact keyed place on the
//! frame. Fixed 100 ms steps; no wall clock.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::CameraPlugin;
use regia_framework::camera::asset::CameraAsset;
use regia_framework::camera::director::CameraHooks;
use regia_framework::camera::driver::{CameraAssets, CameraDriver};
use regia_framework::sequencer::{
    Binding, Channel, Ease, ScalarKey, Sequence, SequenceBindings, SequencePlayer, SequencerPlugin, Track,
};
use regia_scene::components::Name;
use regia_scene::spline::{Spline, SplineCurve};
use regia_scene::{GlobalTransform, Transform};

const STEP: Duration = Duration::from_millis(100);

const ASSET: &str = r#"(format_version: 2, name: "rc",
    variables: [
        (name: "rail_position", default: Float(0.0)),
        (name: "crane_yaw", default: Float(0.0)),
        (name: "crane_length", default: Float(3.0)),
    ],
    rigs: [
        (name: "dolly", root: [Rail(spline: "track", drive: Value(Var("rail_position")))]),
        (name: "crane", root: [Crane(base: Ground((0.0, 1.0, 0.0)), yaw: Var("crane_yaw"),
            length: Var("crane_length"), limits: (yaw_deg: Some((-60.0, 60.0))))]),
    ],
    director: Fixed("dolly"))"#;

fn track() -> Spline {
    Spline::through([
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(10.0, 1.0, 0.0),
        Vec3::new(14.0, 1.0, -8.0),
        Vec3::new(14.0, 1.0, -30.0),
    ])
}

fn keys(points: &[(f32, f32)]) -> Channel {
    Channel::Float(
        points
            .iter()
            .map(|&(t, v)| ScalarKey::new(t, v, Ease::Linear))
            .collect(),
    )
}

fn track_of(property: &str, points: &[(f32, f32)]) -> Track {
    Track {
        name: property.into(),
        target: "cam".into(),
        property: property.into(),
        keys: keys(points),
        enabled: true,
        locked: false,
    }
}

fn setup(tracks: Vec<Track>, director_rig: Option<&str>) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        CameraPlugin,
        regia_physics::PhysicsPlugin,
        SequencerPlugin,
    ))
    .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
    let mut asset = CameraAsset::from_ron(ASSET).expect("asset parses");
    if let Some(rig) = director_rig {
        asset.director = regia_framework::camera::asset::DirectorDef::Fixed(rig.into());
    }
    app.world_mut()
        .resource_mut::<CameraAssets>()
        .insert(asset, None, &CameraHooks::default());
    app.world_mut().spawn((
        Name::new("track"),
        track(),
        Transform::default(),
        GlobalTransform::IDENTITY,
    ));
    let camera = app
        .world_mut()
        .spawn((Transform::default(), CameraDriver::new("rc")))
        .id();
    app.world_mut()
        .resource_scope(|world, assets: Mut<CameraAssets>| {
            world
                .get_mut::<CameraDriver>(camera)
                .expect("driver")
                .bind(&assets);
        });
    let mut player = SequencePlayer::new(Sequence {
        tracks,
        ..Sequence::default()
    });
    player.play();
    app.world_mut().spawn((
        player,
        SequenceBindings::default().with("cam", Binding::Entity(camera)),
    ));
    (app, camera)
}

#[test]
fn a_keyed_sequence_moves_the_dolly_to_exact_positions_at_given_frames() {
    // 0 m at 0 s, 10 m at 1 s, hold 10 m, then 25 m at 2 s: three segments, two speeds.
    let (mut app, camera) = setup(
        vec![track_of(
            "camera.rail.position",
            &[(0.0, 0.0), (1.0, 10.0), (2.0, 25.0)],
        )],
        None,
    );
    let curve = SplineCurve::new(&track(), None);
    // The first update already advances the clock by one step: after `frame` updates, t = frame · 0.1 s.
    for frame in 1..=20u32 {
        app.update();
        let t = frame as f32 * 0.1;
        let expected_m = if t <= 1.0 {
            10.0 * t
        } else {
            10.0 + 15.0 * (t - 1.0)
        };
        let driver = app.world().get::<CameraDriver>(camera).expect("driver");
        let rails = driver.core().expect("bound").rig_infos().remove(0).rails;
        let report = rails.first().expect("the rail node ran");
        assert!(
            (report.distance_m - expected_m).abs() < 1.0e-3,
            "frame {frame}: {} m vs {expected_m} m",
            report.distance_m
        );
        let at = app
            .world()
            .get::<Transform>(camera)
            .expect("transform")
            .translation;
        assert!(
            (at - curve.at_distance(expected_m).position).length() < 1.0e-3,
            "frame {frame}: {at}"
        );
    }
}

#[test]
fn a_keyed_sequence_swings_the_crane_within_its_limits() {
    let (mut app, camera) = setup(
        vec![
            track_of("camera.crane.yaw", &[(0.0, 0.0), (1.0, 120.0)]),
            track_of("camera.crane.length", &[(0.0, 3.0), (1.0, 3.0)]),
        ],
        Some("crane"),
    );
    for frame in 1..=10u32 {
        app.update();
        let requested = 120.0 * frame as f32 * 0.1;
        let driver = app.world().get::<CameraDriver>(camera).expect("driver");
        let cranes = driver.core().expect("bound").rig_infos().remove(0).cranes;
        let report = cranes.first().expect("the crane node ran");
        assert!(
            (report.requested.yaw_deg - requested).abs() < 1.0e-3,
            "frame {frame}"
        );
        assert!(
            (report.axes.yaw_deg - requested.min(60.0)).abs() < 1.0e-3,
            "frame {frame}"
        );
        assert_eq!(report.clamped[0], requested > 60.0, "frame {frame}");
        assert!(
            (report.tip - app.world().get::<Transform>(camera).expect("t").translation).length() < 1.0e-4
        );
    }
}

#[test]
fn a_track_for_an_undeclared_variable_reports_instead_of_writing_nothing_silently() {
    // The asset has no `crane_pitch`: the track is skipped and the player lists the issue.
    let (mut app, _) = setup(
        vec![track_of("camera.crane.pitch", &[(0.0, 0.0), (1.0, 10.0)])],
        None,
    );
    app.update();
    app.update();
    app.update();
    let mut players = app.world_mut().query::<&SequencePlayer>();
    let player = players.single(app.world()).expect("one player");
    assert!(
        player.issues().next().is_some(),
        "an apply error is reported once for the undeclared variable"
    );
}
