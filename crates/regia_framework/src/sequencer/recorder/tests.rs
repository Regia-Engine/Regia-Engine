// SPDX-License-Identifier: MPL-2.0
//! Take recorder tests (ADR-2637): record scripted motion at a fixed step, play the take back
//! through a real `SequencePlayer`, and compare at every recorded step.

use std::time::Duration;

use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_render::{Camera, PointLight};
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Visibility};

use super::*;
use crate::sequencer::asset::{Ease, Track, Vec3Key};
use crate::sequencer::player::{ClockSource, EndPolicy, SequenceBindings, SequencePlayer, SequencerPlugin};

/// One fixed step at 60 Hz.
const STEP: Duration = Duration::from_nanos(16_666_667);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SequencerPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
    app.register_type::<PointLight>();
    app
}

/// The scripted motion: a known curve of the take time `t` (seconds).
fn scripted(t: f32) -> Transform {
    Transform {
        translation: Vec3::new(3.0 * (1.3 * t).sin(), 0.5 * t * t, 2.0 * (0.7 * t).cos()),
        rotation: Quat::from_euler(EulerRot::YXZ, 2.5 * t, 0.4 * (2.0 * t).sin(), 0.2 * t),
        scale: Vec3::splat(1.0 + 0.25 * (3.0 * t).sin()),
    }
}

/// The poses the mover wrote, one per gameplay frame that advanced time (and the first).
#[derive(Resource, Default)]
struct Truth {
    start: Option<Duration>,
    frames: Vec<(Duration, Transform)>,
}

#[derive(Component)]
struct Mover;

/// Moves every `Mover` on the scripted curve of the gameplay time since the first frame, and
/// keeps the truth.
fn move_scripted(time: Res<Time>, mut truth: ResMut<Truth>, mut movers: Query<&mut Transform, With<Mover>>) {
    let now = time.elapsed();
    let start = *truth.start.get_or_insert(now);
    if !truth.frames.is_empty() && time.delta().is_zero() {
        return;
    }
    let t = now - start;
    let pose = scripted(t.as_secs_f32());
    for mut transform in &mut movers {
        *transform = pose;
    }
    truth.frames.push((t, pose));
}

fn binding(name: &str, entity: Entity, channels: Vec<TakeChannel>) -> TakeBinding {
    TakeBinding {
        name: name.into(),
        target: TakeTarget::Binding(Binding::Entity(entity)),
        channels,
    }
}

fn start(app: &mut App, config: TakeConfig) {
    app.world_mut()
        .resource_mut::<TakeRecorder>()
        .start(config)
        .expect("start");
}

fn stop(app: &mut App) -> RecordedTake {
    app.world_mut()
        .resource_mut::<TakeRecorder>()
        .stop()
        .expect("stop")
}

fn track<'a>(take: &'a Sequence, name: &str) -> &'a Track {
    take.tracks.iter().find(|t| t.name == name).unwrap_or_else(|| {
        panic!(
            "no track {name} in {:?}",
            take.tracks.iter().map(|t| &t.name).collect::<Vec<_>>()
        )
    })
}

/// A player of `take` on an external clock, its targets bound to the given entities.
fn spawn_player(app: &mut App, take: Sequence, bindings: &[(&str, Entity)]) -> Entity {
    let mut player = SequencePlayer::new(take);
    player.clock = ClockSource::External;
    player.end = EndPolicy::Hold;
    player.takeover.enabled = false;
    player.play();
    let mut table = SequenceBindings::default();
    for (name, entity) in bindings {
        table.bind(*name, Binding::Entity(*entity));
    }
    app.world_mut().spawn((player, table)).id()
}

fn show(app: &mut App, player: Entity, seconds: f64) {
    app.world_mut()
        .get_mut::<SequencePlayer>(player)
        .expect("player")
        .set_external_time(seconds);
    app.update();
}

fn angle_degrees(a: Quat, b: Quat) -> f32 {
    a.normalize().angle_between(b.normalize()).to_degrees()
}

#[test]
fn a_scripted_curve_replays_within_the_tolerance_at_every_step() {
    let mut app = app();
    app.init_resource::<Truth>().add_systems(Update, move_scripted);
    let mover = app.world_mut().spawn((Transform::default(), Mover)).id();
    let tolerance = TakeTolerance::default();
    start(
        &mut app,
        TakeConfig {
            bindings: vec![binding("mover", mover, vec![TakeChannel::Transform])],
            tolerance,
            ..TakeConfig::default()
        },
    );
    const STEPS: usize = 240;
    for _ in 0..STEPS {
        app.update();
    }
    let take = stop(&mut app);
    let truth = std::mem::take(&mut app.world_mut().resource_mut::<Truth>().frames);
    assert_eq!(take.frames as usize, truth.len(), "one sample per frame of truth");
    assert!(truth.len() >= STEPS - 1);
    let position = track(&take.sequence, "mover/transform.position");
    assert_eq!(position.property, "transform.position");
    assert!(
        position.keys.len() < truth.len() / 2,
        "a smooth curve reduces: {} keys for {} samples",
        position.keys.len(),
        truth.len()
    );

    let mut replay = self::app();
    let ghost = replay.world_mut().spawn(Transform::default()).id();
    let player = spawn_player(&mut replay, take.sequence.clone(), &[("mover", ghost)]);
    for (i, (t, expected)) in truth.iter().enumerate() {
        show(&mut replay, player, t.as_secs_f64());
        let got = *replay.world().get::<Transform>(ghost).expect("transform");
        let distance = got.translation.distance(expected.translation);
        assert!(
            distance <= tolerance.position + 1.0e-5,
            "step {i}: position off by {distance} m (tolerance {})",
            tolerance.position
        );
        let angle = angle_degrees(got.rotation, expected.rotation);
        assert!(
            angle <= tolerance.rotation + 2.0e-3,
            "step {i}: rotation off by {angle} degrees (tolerance {})",
            tolerance.rotation
        );
        let scale = (got.scale - expected.scale).abs().max_element();
        assert!(
            scale <= tolerance.scale + 1.0e-6,
            "step {i}: scale off by {scale}"
        );
    }
}

#[test]
fn a_still_object_writes_two_keys_per_track() {
    let mut app = app();
    let still = app
        .world_mut()
        .spawn(Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::from_rotation_y(0.5),
            scale: Vec3::splat(2.0),
        })
        .id();
    start(
        &mut app,
        TakeConfig {
            bindings: vec![binding("still", still, vec![TakeChannel::Transform])],
            ..TakeConfig::default()
        },
    );
    for _ in 0..90 {
        app.update();
    }
    let take = stop(&mut app);
    for property in ["transform.position", "transform.rotation", "transform.scale"] {
        let keys = &track(&take.sequence, &format!("still/{property}")).keys;
        assert_eq!(keys.len(), 2, "{property}: a still object writes two keys");
        let Channel::Vec3(keys) = keys else {
            panic!("{property} is a Vec3 track");
        };
        assert_eq!(keys[0].time, 0.0);
        assert!(keys[1].time > 1.4, "the last key is at the end of the take");
    }
}

#[test]
fn properties_and_a_reflected_field_survive_the_file_round_trip() {
    #[derive(Resource, Default)]
    struct Frame(u32);
    fn animate(mut frame: ResMut<Frame>, mut lights: Query<(&mut PointLight, &mut Visibility)>) {
        frame.0 += 1;
        let k = frame.0 as f32;
        for (mut light, mut visibility) in &mut lights {
            light.intensity = 10.0 + 0.5 * k;
            light.radius = 5.0 + (0.2 * k).sin();
            *visibility = if frame.0 >= 30 {
                Visibility::Hidden
            } else {
                Visibility::Visible
            };
        }
    }
    let mut app = app();
    app.init_resource::<Frame>().add_systems(Update, animate);
    let lamp = app
        .world_mut()
        .spawn((Transform::default(), PointLight::default(), Visibility::Visible))
        .id();
    #[derive(Resource, Default)]
    struct Seen(Vec<(f32, f32, bool)>);
    fn watch(lights: Query<(&PointLight, &Visibility)>, mut seen: ResMut<Seen>) {
        for (light, visibility) in &lights {
            seen.0
                .push((light.intensity, light.radius, *visibility != Visibility::Hidden));
        }
    }
    app.init_resource::<Seen>()
        .add_systems(PostUpdate, watch.before(SequencerSet::Advance));
    start(
        &mut app,
        TakeConfig {
            bindings: vec![binding(
                "lamp",
                lamp,
                vec![
                    TakeChannel::parse("property:light.intensity").expect("channel"),
                    TakeChannel::parse("property:visible").expect("channel"),
                    TakeChannel::parse("reflect:PointLight.radius").expect("channel"),
                ],
            )],
            ..TakeConfig::default()
        },
    );
    for _ in 0..60 {
        app.update();
    }
    let take = stop(&mut app);
    let seen = std::mem::take(&mut app.world_mut().resource_mut::<Seen>().0);
    assert_eq!(take.frames as usize, seen.len());
    assert!(take.issues.is_empty(), "{:?}", take.issues);
    let intensity = track(&take.sequence, "lamp/light.intensity");
    assert_eq!(intensity.keys.len(), 2, "a linear ramp is two keys");
    let Channel::Bool(visible) = &track(&take.sequence, "lamp/visible").keys else {
        panic!("visible is a Bool track");
    };
    assert_eq!(visible.len(), 3, "first, the change, and the last sample");

    // Through the file: save next to a source, load the take back from disk.
    let root = regia_test_utils::unique_temp_dir("take-props");
    let files = save_take(&root, "cine/lamp.seq.ron", &take.sequence, 0.0).expect("save");
    let text = std::fs::read_to_string(root.join(&files.take)).expect("read take");
    let loaded = Sequence::from_ron(&text).expect("the take loads");
    assert_eq!(loaded.version, SEQUENCE_FORMAT_VERSION);
    assert_eq!(loaded.tracks, take.sequence.tracks, "tracks survive the file");

    let mut replay = self::app();
    let ghost = replay
        .world_mut()
        .spawn((Transform::default(), PointLight::default(), Visibility::Visible))
        .id();
    let player = spawn_player(&mut replay, loaded, &[("lamp", ghost)]);
    let tolerance = TakeTolerance::default().property;
    for (i, (intensity, radius, visible)) in seen.iter().enumerate() {
        show(&mut replay, player, i as f64 * STEP.as_secs_f64());
        let light = replay.world().get::<PointLight>(ghost).expect("light");
        assert!(
            (light.intensity - intensity).abs() <= tolerance + 1.0e-5,
            "step {i}: intensity {} vs {intensity}",
            light.intensity
        );
        assert!(
            (light.radius - radius).abs() <= tolerance + 1.0e-6,
            "step {i}: radius {} vs {radius}",
            light.radius
        );
        let shown = *replay.world().get::<Visibility>(ghost).expect("visibility") != Visibility::Hidden;
        assert_eq!(shown, *visible, "step {i}: visibility");
    }
    let player = replay.world().get::<SequencePlayer>(player).expect("player");
    assert_eq!(
        player.issues().count(),
        0,
        "{:?}",
        player.issues().collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_camera_cut_becomes_a_step_and_a_marker_and_replays_without_a_blend() {
    #[derive(Resource, Default)]
    struct Frame(u32);
    #[derive(Component)]
    struct Second;
    fn cut_at_30(mut frame: ResMut<Frame>, mut cameras: Query<(&mut Camera, &mut Transform, Has<Second>)>) {
        frame.0 += 1;
        for (mut camera, mut transform, second) in &mut cameras {
            if second {
                camera.order = if frame.0 >= 30 { 5 } else { -5 };
                transform.translation = Vec3::new(50.0, 2.0, 0.0);
            } else {
                // The first camera dollies slowly.
                transform.translation = Vec3::new(0.01 * frame.0 as f32, 1.0, 0.0);
            }
        }
    }
    let mut app = app();
    app.init_resource::<Frame>().add_systems(Update, cut_at_30);
    let first_camera = Camera {
        fov_y: 50f32.to_radians(),
        ..Camera::default()
    };
    let second_camera = Camera {
        fov_y: 30f32.to_radians(),
        order: -5,
        ..Camera::default()
    };
    app.world_mut().spawn((first_camera, Transform::default()));
    app.world_mut()
        .spawn((second_camera, Transform::default(), Second));
    start(
        &mut app,
        TakeConfig {
            bindings: vec![TakeBinding {
                name: "camera".into(),
                target: TakeTarget::ActiveCamera,
                channels: vec![TakeChannel::Camera],
            }],
            ..TakeConfig::default()
        },
    );
    for _ in 0..60 {
        app.update();
    }
    let take = stop(&mut app);
    let marker = take.sequence.marker("camera/cut_1").expect("a cut marker");
    assert_eq!(marker.payload, "camera_cut");
    let Channel::Vec3(keys) = &track(&take.sequence, "camera/transform.position").keys else {
        panic!("position is Vec3");
    };
    let cut = keys
        .iter()
        .position(|k| k.time == marker.time)
        .expect("a key at the cut");
    assert!(cut > 0);
    assert_eq!(keys[cut - 1].ease, Ease::Step, "the key before the cut steps");
    assert_eq!(keys[cut].value, [50.0, 2.0, 0.0]);

    // Round trip: before the cut (even half a step before) the first camera, after it the second.
    let mut replay = self::app();
    let ghost = replay
        .world_mut()
        .spawn((Camera::default(), Transform::default()))
        .id();
    let player = spawn_player(&mut replay, take.sequence.clone(), &[("camera", ghost)]);
    let at = f64::from(marker.time);
    let step = STEP.as_secs_f64();
    for (t, expect_second) in [
        (at - step, false),
        (at - 0.5 * step, false),
        (at, true),
        (at + step, true),
    ] {
        show(&mut replay, player, t);
        let pose = *replay.world().get::<Transform>(ghost).expect("transform");
        let fov = replay
            .world()
            .get::<Camera>(ghost)
            .expect("camera")
            .fov_y
            .to_degrees();
        if expect_second {
            assert_eq!(pose.translation, Vec3::new(50.0, 2.0, 0.0), "at {t}");
            assert!((fov - 30.0).abs() < 1.0e-3, "fov {fov} at {t}");
        } else {
            assert!(
                pose.translation.x < 1.0,
                "no blend toward the second camera at {t}: {pose:?}"
            );
            assert!((fov - 50.0).abs() < 1.0e-3, "fov {fov} at {t}");
        }
    }
}

/// A sequence that drives `hero`'s position.
fn hero_sequence() -> Sequence {
    Sequence {
        tracks: vec![Track {
            name: "walk".into(),
            target: "hero".into(),
            property: "transform.position".into(),
            keys: Channel::Vec3(vec![
                Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Linear),
                Vec3Key::new(4.0, [8.0, 0.0, 0.0], Ease::Linear),
            ]),
            enabled: true,
            locked: false,
        }],
        ..Sequence::default()
    }
}

#[test]
fn a_playing_sequence_s_own_bindings_are_left_out_unless_asked() {
    for include_driven in [false, true] {
        let mut app = app();
        app.init_resource::<Truth>().add_systems(Update, move_scripted);
        let hero = app.world_mut().spawn(Transform::default()).id();
        let free = app.world_mut().spawn((Transform::default(), Mover)).id();
        let mut player = SequencePlayer::new(hero_sequence());
        player.play();
        app.world_mut().spawn((
            player,
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        app.update();
        start(
            &mut app,
            TakeConfig {
                bindings: vec![
                    binding("hero", hero, vec![TakeChannel::Transform]),
                    binding("free", free, vec![TakeChannel::Transform]),
                ],
                include_driven,
                ..TakeConfig::default()
            },
        );
        for _ in 0..30 {
            app.update();
        }
        let state = app.world().resource::<TakeRecorder>().binding_states();
        assert_eq!(state[0].excluded, (!include_driven).then_some("driven"));
        let take = stop(&mut app);
        let has_hero = take.sequence.tracks.iter().any(|t| t.target == "hero");
        assert_eq!(has_hero, include_driven, "include_driven {include_driven}");
        assert!(take.sequence.tracks.iter().any(|t| t.target == "free"));
        if include_driven {
            let Channel::Vec3(keys) = &track(&take.sequence, "hero/transform.position").keys else {
                panic!("Vec3");
            };
            assert!(
                keys.last().expect("keys").value[0] > 0.5,
                "the sequence's walk is recorded"
            );
        } else {
            assert_eq!(take.excluded, vec![("hero".to_string(), "driven".to_string())]);
        }
    }
}

#[test]
fn the_countdown_waits_whole_frames_and_cues_become_audio_sections() {
    let mut app = app();
    let still = app.world_mut().spawn(Transform::default()).id();
    start(
        &mut app,
        TakeConfig {
            bindings: vec![binding("still", still, vec![TakeChannel::Transform])],
            countdown: 5,
            audio_cues: true,
            ..TakeConfig::default()
        },
    );
    app.update(); // the first frame may not advance gameplay time
    let mut counted = 0;
    while app.world().resource::<TakeRecorder>().status() == TakeStatus::Countdown {
        app.world_mut().write_message(TakeCue::new("ignored", 1.0));
        app.update();
        counted += 1;
        assert!(counted <= 6, "the countdown ends");
    }
    assert_eq!(
        app.world().resource::<TakeRecorder>().status(),
        TakeStatus::Recording
    );
    assert_eq!(
        app.world().resource::<TakeRecorder>().frames(),
        1,
        "the first sample is the frame after the countdown"
    );
    for frame in 0..20 {
        if frame == 10 {
            let mut cue = TakeCue::new("door", 1.5);
            cue.volume_db = -6.0;
            cue.source = Some(SoundSource::Binding("still".into()));
            app.world_mut().write_message(cue);
        }
        app.update();
    }
    let recorder = app.world().resource::<TakeRecorder>();
    assert_eq!(recorder.cues(), 1, "cues during the countdown are not recorded");
    let take = stop(&mut app);
    let Channel::Audio(sections) = &track(&take.sequence, "audio").keys else {
        panic!("an audio track");
    };
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].sound, "door");
    assert_eq!(sections[0].volume_db, -6.0);
    assert!(sections[0].start > 0.0);
    assert!((sections[0].end - sections[0].start - 1.5).abs() < 1.0e-4);
    assert_eq!(
        sections[0].spatial.as_ref().map(|s| s.source.clone()),
        Some(SoundSource::Binding("still".into()))
    );
}

#[test]
fn guid_bindings_resolve_and_a_missing_one_is_counted() {
    let mut app = app();
    let guid = Guid::from_u128(0x77);
    app.world_mut().spawn((Transform::default(), EntityGuid(guid)));
    start(
        &mut app,
        TakeConfig {
            bindings: vec![
                TakeBinding {
                    name: "hero".into(),
                    target: TakeTarget::Binding(Binding::Guid(guid)),
                    channels: vec![TakeChannel::Transform],
                },
                TakeBinding {
                    name: "ghost".into(),
                    target: TakeTarget::Binding(Binding::Guid(Guid::from_u128(0x78))),
                    channels: vec![TakeChannel::Transform],
                },
            ],
            ..TakeConfig::default()
        },
    );
    for _ in 0..4 {
        app.update();
    }
    let states = app.world().resource::<TakeRecorder>().binding_states();
    assert!(states[0].entity.is_some());
    assert_eq!(states[1].missing, states[0].frames);
    let take = stop(&mut app);
    assert!(take.sequence.tracks.iter().all(|t| t.target == "hero"));
}

#[test]
fn start_and_stop_refuse_with_codes() {
    let mut recorder = TakeRecorder::default();
    assert_eq!(
        recorder.stop().map(|_| ()).expect_err("refused").code(),
        "take_not_recording"
    );
    assert_eq!(
        recorder.start(TakeConfig::default()).expect_err("refused").code(),
        "take_no_bindings"
    );
    let entity = World::new().spawn_empty().id();
    let twice = TakeConfig {
        bindings: vec![
            binding("a", entity, vec![TakeChannel::Transform]),
            binding("a", entity, vec![TakeChannel::Transform]),
        ],
        ..TakeConfig::default()
    };
    assert_eq!(
        recorder.start(twice).expect_err("refused").code(),
        "take_bad_binding"
    );
    let both = TakeConfig {
        bindings: vec![binding(
            "a",
            entity,
            vec![TakeChannel::Transform, TakeChannel::Camera],
        )],
        ..TakeConfig::default()
    };
    assert_eq!(
        recorder.start(both).expect_err("refused").code(),
        "take_bad_channel"
    );
    let loose = TakeConfig {
        bindings: vec![binding("a", entity, vec![TakeChannel::Transform])],
        tolerance: TakeTolerance {
            position: 0.0,
            ..TakeTolerance::default()
        },
        ..TakeConfig::default()
    };
    assert_eq!(
        recorder.start(loose).expect_err("refused").code(),
        "take_bad_tolerance"
    );
    let fine = TakeConfig {
        bindings: vec![binding("a", entity, vec![TakeChannel::Transform])],
        countdown: 3,
        ..TakeConfig::default()
    };
    recorder.start(fine.clone()).expect("start");
    assert_eq!(recorder.start(fine).expect_err("refused").code(), "take_busy");
    assert_eq!(
        recorder.stop().map(|_| ()).expect_err("refused").code(),
        "take_empty"
    );
    assert_eq!(
        recorder.status(),
        TakeStatus::Idle,
        "a refused stop still ends the take"
    );
    assert_eq!(
        TakeChannel::parse("wings").expect_err("refused").code(),
        "take_bad_channel"
    );
    assert_eq!(
        TakeChannel::parse("property:").expect_err("refused").code(),
        "take_bad_channel"
    );
    assert_eq!(
        TakeChannel::parse("reflect:Glow.amount").expect("reflect"),
        TakeChannel::Property("reflect:Glow.amount".into())
    );
}

#[test]
fn rdp_keeps_ends_corners_and_window_edges() {
    let times: Vec<f32> = (0..11).map(|i| i as f32).collect();
    let flat = vec![[1.0f32]; 11];
    assert_eq!(
        reduce_samples(&times, &flat, ErrorMetric::Component(1.0e-3)),
        vec![0, 10]
    );
    let line: Vec<[f32; 1]> = times.iter().map(|t| [2.0 * t]).collect();
    assert_eq!(
        reduce_samples(&times, &line, ErrorMetric::Component(1.0e-3)),
        vec![0, 10]
    );
    let corner: Vec<[f32; 1]> = times.iter().map(|t| [(t - 4.0).abs()]).collect();
    assert_eq!(
        reduce_samples(&times, &corner, ErrorMetric::Component(1.0e-3)),
        vec![0, 4, 10]
    );
    // A loose tolerance swallows the corner.
    assert_eq!(
        reduce_samples(&times, &corner, ErrorMetric::Component(10.0)),
        vec![0, 10]
    );
    assert_eq!(
        reduce_samples(&times[..1], &flat[..1], ErrorMetric::Component(1.0)),
        vec![0]
    );
    let long: Vec<f32> = (0..5000).map(|i| i as f32 * 0.01).collect();
    let still = vec![[0.0f32, 0.0, 0.0]; 5000];
    assert_eq!(
        reduce_samples(&long, &still, ErrorMetric::Distance(1.0e-3)),
        vec![0, 2048, 4096, 4999],
        "window edges are kept"
    );
}

#[test]
fn euler_unwrap_crosses_the_seam_without_a_long_way_round() {
    assert_eq!(
        unwrap_degrees([0.0, -179.0, 0.0], [0.0, 179.0, 0.0]),
        [0.0, 181.0, 0.0]
    );
    assert_eq!(
        unwrap_degrees([10.0, 20.0, 30.0], [370.0, 20.0, -330.0]),
        [370.0, 20.0, -330.0]
    );
    let q = Quat::from_euler(EulerRot::YXZ, 1.0, 0.3, -0.2);
    let back = euler_degrees_to_rotation(rotation_to_euler_degrees(q));
    assert!(angle_degrees(q, back) < 1.0e-3);
}

#[test]
fn clip_sections_split_on_a_change_of_clip_or_a_wrap() {
    let frame = |i: u64, clip: &str, t: f32| ClipFrame {
        time: i * 100_000_000,
        clip: clip.into(),
        clip_time: t,
    };
    let frames = vec![
        frame(0, "walk", 0.0),
        frame(1, "walk", 0.1),
        frame(2, "walk", 0.2),
        frame(3, "walk", 0.0), // wrapped
        frame(4, "walk", 0.1),
        frame(5, "run", 0.5),
        frame(6, "run", 0.7),
    ];
    let sections = clip_sections(&frames, 1.0 / 120.0, 0.1);
    let summary: Vec<(&str, f32, f32, f32, f32)> = sections
        .iter()
        .map(|s| (s.clip.as_str(), s.start, s.end, s.offset, s.speed))
        .collect();
    assert_eq!(summary.len(), 3, "{summary:?}");
    assert_eq!(summary[0].0, "walk");
    assert!((summary[0].2 - 0.3).abs() < 1.0e-6);
    assert!((summary[0].4 - 1.0).abs() < 1.0e-4);
    assert_eq!(summary[1].3, 0.0);
    assert_eq!(summary[2].0, "run");
    assert!((summary[2].4 - 2.0).abs() < 1.0e-3, "run plays at double speed");
    assert!(
        (summary[2].2 - 0.7).abs() < 1.0e-6,
        "the last section ends a frame after its sample"
    );
}

#[test]
fn takes_are_numbered_next_to_the_source_and_added_as_sections() {
    let root = regia_test_utils::unique_temp_dir("take-files");
    let take = Sequence {
        tracks: vec![Track {
            name: "hero/transform.position".into(),
            target: "hero".into(),
            property: "transform.position".into(),
            keys: Channel::Vec3(vec![
                Vec3Key::new(0.0, [0.0; 3], Ease::Linear),
                Vec3Key::new(2.0, [1.0; 3], Ease::Linear),
            ]),
            enabled: true,
            locked: false,
        }],
        ..Sequence::default()
    };
    // A missing source is created with just the take's section.
    let first = save_take(&root, "cine/intro.seq.ron", &take, 0.0).expect("save");
    assert_eq!(first.take, "cine/intro_take1.seq.ron");
    assert_eq!(first.number, 1);
    assert!(first.created_source);
    // An existing source keeps its tracks and gains a second section.
    let mut source =
        Sequence::from_ron(&std::fs::read_to_string(root.join("cine/intro.seq.ron")).expect("read"))
            .expect("source");
    source.tracks.push(crate::sequencer::asset::fixtures::float_track(
        "fade", "ui", "opacity", 1.0,
    ));
    std::fs::write(root.join("cine/intro.seq.ron"), source.to_ron().expect("ron")).expect("write");
    let second = save_take(&root, "cine/intro.seq.ron", &take, 1.5).expect("save");
    assert_eq!(second.take, "cine/intro_take2.seq.ron");
    assert!(!second.created_source);
    let source = Sequence::from_ron(&std::fs::read_to_string(root.join("cine/intro.seq.ron")).expect("read"))
        .expect("source");
    let names: Vec<&str> = source.tracks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["take1", "fade", "take2"], "nothing is overwritten");
    let Channel::Sub(sections) = &source.tracks[2].keys else {
        panic!("a sub-sequence track");
    };
    assert_eq!(sections[0].sequence, "cine/intro_take2.seq.ron");
    assert_eq!((sections[0].start, sections[0].end), (1.5, 3.5));
    let written =
        Sequence::from_ron(&std::fs::read_to_string(root.join(&second.take)).expect("read")).expect("take");
    assert_eq!(written.name, "intro_take2");
    // The source plays its takes through a library.
    let mut library = crate::sequencer::SequenceLibrary::default();
    library.insert(first.take.clone(), written.clone());
    library.insert(second.take.clone(), written);
    library.check(&source).expect("the tree resolves");
    let frame = source.sample_with(2.5, &library);
    assert!(
        frame.values.iter().any(|v| v.target == "hero"),
        "the take plays inside the source"
    );

    assert_eq!(
        save_take(&root, "../x.seq.ron", &take, 0.0)
            .expect_err("refused")
            .code(),
        "take_bad_path"
    );
    assert_eq!(
        save_take(&root, "x.ron", &take, 0.0).expect_err("refused").code(),
        "take_bad_path"
    );
    std::fs::write(root.join("broken.seq.ron"), "(version: 3, tracks: [(").expect("write");
    assert_eq!(
        save_take(&root, "broken.seq.ron", &take, 0.0)
            .expect_err("refused")
            .code(),
        "take_source_invalid"
    );
    assert!(
        !root.join("broken_take1.seq.ron").exists(),
        "a refused save writes nothing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_reflected_property_applies_through_the_player_fallback() {
    let mut app = app();
    let lamp = app.world_mut().spawn(PointLight::default()).id();
    let sequence = Sequence {
        tracks: vec![Track {
            name: "radius".into(),
            target: "lamp".into(),
            property: "reflect:PointLight.radius".into(),
            keys: Channel::Float(vec![
                crate::sequencer::asset::ScalarKey::new(0.0, 2.0, Ease::Linear),
                crate::sequencer::asset::ScalarKey::new(1.0, 4.0, Ease::Linear),
            ]),
            enabled: true,
            locked: false,
        }],
        ..Sequence::default()
    };
    let player = spawn_player(&mut app, sequence, &[("lamp", lamp)]);
    show(&mut app, player, 0.5);
    assert!((app.world().get::<PointLight>(lamp).expect("light").radius - 3.0).abs() < 1.0e-5);
    // A field that does not take the value is reported, not panicked on.
    let bad = Sequence {
        tracks: vec![Track {
            name: "nope".into(),
            target: "lamp".into(),
            property: "reflect:PointLight.cast_shadows".into(),
            keys: Channel::Float(vec![crate::sequencer::asset::ScalarKey::new(
                0.0,
                1.0,
                Ease::Linear,
            )]),
            enabled: true,
            locked: false,
        }],
        ..Sequence::default()
    };
    let player = spawn_player(&mut app, bad, &[("lamp", lamp)]);
    show(&mut app, player, 0.0);
    let issues: Vec<String> = app
        .world()
        .get::<SequencePlayer>(player)
        .expect("player")
        .issues()
        .map(|i| i.kind().to_string())
        .collect();
    assert_eq!(issues, ["apply_failed"]);
}

#[test]
fn colliding_track_names_get_a_suffix_and_the_take_still_validates() {
    // `property:clip` writes the track `hero/clip`, the name the animation channel's clip track has.
    let hero = World::new().spawn_empty().id();
    let mut recorder = TakeRecorder::default();
    recorder
        .start(TakeConfig {
            bindings: vec![binding(
                "hero",
                hero,
                vec![TakeChannel::Animation, TakeChannel::Property("clip".into())],
            )],
            ..TakeConfig::default()
        })
        .expect("start");
    recorder.frames = 2;
    let recording = &mut recorder.bindings[0];
    for (i, value) in [(0u64, 1.0f32), (1, 2.0)] {
        let time = i * 100_000_000;
        recording
            .track("clip", || {
                RecordedTrack::Float(Samples::default(), ErrorMetric::Component(1.0e-4))
            })
            .push(time, ChannelValue::Float(value))
            .expect("pushed");
        recording.clips.push(ClipFrame {
            time,
            clip: "walk".into(),
            clip_time: 0.1 * i as f32,
        });
    }
    let take = recorder.stop().expect("stop");
    let names: Vec<&str> = take.sequence.tracks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["hero/clip", "hero/clip#2"]);
    assert_eq!(take.tracks[1].0, "hero/clip#2", "the summary follows the rename");
    take.sequence.validate().expect("valid");
}

#[test]
fn countdown_frames_are_exact_and_stale_cues_stay_out() {
    let mut recorder = TakeRecorder::default();
    let world = World::new();
    let entity = World::new().spawn_empty().id();
    recorder
        .start(TakeConfig {
            bindings: vec![binding("e", entity, vec![TakeChannel::Transform])],
            countdown: 3,
            ..TakeConfig::default()
        })
        .expect("start");
    recorder.frame(&world, 0);
    assert_eq!(
        recorder.countdown_left(),
        3,
        "a frame that does not advance time does not count"
    );
    for left in [2, 1, 0] {
        recorder.frame(&world, 10);
        assert_eq!(recorder.countdown_left(), left);
        assert_eq!(
            recorder.status(),
            TakeStatus::Countdown,
            "{left} left: still waiting"
        );
    }
    recorder.frame(&world, 10);
    assert_eq!(recorder.status(), TakeStatus::Recording);
    assert_eq!(recorder.frames(), 1, "three frames waited, the fourth sampled");

    // A cue sent while idle is drained by the collector and never reaches the next take.
    let mut app = app();
    let still = app.world_mut().spawn(Transform::default()).id();
    app.world_mut().write_message(TakeCue::new("stale", 1.0));
    app.update();
    start(
        &mut app,
        TakeConfig {
            bindings: vec![binding("still", still, vec![TakeChannel::Transform])],
            audio_cues: true,
            ..TakeConfig::default()
        },
    );
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<TakeRecorder>().cues(), 0);
}

#[test]
fn a_non_finite_sample_is_dropped_with_an_issue_and_the_take_survives() {
    let mut track = RecordedTrack::Float(Samples::default(), ErrorMetric::Component(1.0e-4));
    assert!(track.push(0, ChannelValue::Float(f32::NAN)).is_err());
    assert!(
        track.push(0, ChannelValue::Vec3([0.0; 3])).is_err(),
        "another kind"
    );
    assert!(track.push(0, ChannelValue::Float(1.0)).is_ok());
    assert_eq!(track.len(), 1);

    let mut app = app();
    let lamp = app
        .world_mut()
        .spawn((Transform::default(), PointLight::default()))
        .id();
    start(
        &mut app,
        TakeConfig {
            bindings: vec![binding(
                "lamp",
                lamp,
                vec![TakeChannel::parse("property:light.intensity").expect("channel")],
            )],
            ..TakeConfig::default()
        },
    );
    for k in 0..6 {
        app.world_mut()
            .get_mut::<PointLight>(lamp)
            .expect("light")
            .intensity = if k == 3 { f32::NAN } else { k as f32 };
        app.update();
    }
    let take = stop(&mut app);
    assert!(
        take.issues.iter().any(|i| i.contains("not finite")),
        "{:?}",
        take.issues
    );
    assert_eq!(take.tracks[0].1, 5, "five finite samples of six");
}

#[test]
fn a_kept_unsaved_take_blocks_the_next_until_saved_or_dropped() {
    let entity = World::new().spawn_empty().id();
    let config = TakeConfig {
        bindings: vec![binding("e", entity, vec![TakeChannel::Transform])],
        ..TakeConfig::default()
    };
    let mut recorder = TakeRecorder::default();
    recorder.keep_unsaved(RecordedTake {
        sequence: Sequence::default(),
        frames: 1,
        duration: 0.0,
        tracks: Vec::new(),
        excluded: Vec::new(),
        dropped_cues: 0,
        issues: Vec::new(),
    });
    assert_eq!(
        recorder.start(config.clone()).expect_err("refused").code(),
        "take_unsaved"
    );
    recorder.cancel().expect("an idle cancel drops the kept take");
    assert!(!recorder.has_unsaved());
    recorder.start(config).expect("start");
}
