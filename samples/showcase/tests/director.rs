// SPDX-License-Identifier: MIT-0
//! The director on the fixed step, without a GPU: the tracked entity follows the track frame by
//! frame, effects and decals are requested on their frames exactly once, the weather changes on
//! its frame, and two runs agree. The renderer is not built, so this runs everywhere.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::prelude::*;
use regia_app::enable_fixed_step;
use regia_scene::Transform;
use regia_vfx::{DecalPool, VfxScene, Weather};
use showcase::STEP_HZ;
use showcase::app::{Options, ShowcaseClock, ShowcasePlugin, ShowcaseStats, Tracked};
use showcase::scene::{SceneData, load_scene};

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

fn scene() -> Arc<SceneData> {
    Arc::new(load_scene(&example_dir()).expect("example scene"))
}

fn app(scene: Arc<SceneData>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, STEP_HZ);
    app.add_plugins(ShowcasePlugin {
        scene,
        options: Options {
            plain: true,
            ..Options::default()
        },
    });
    app
}

/// One record per update: the clock after it, the stats, the pod position.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    clock: i64,
    effects: u32,
    decals: u32,
    changes: u32,
    pod: [f32; 3],
}

fn run(scene: Arc<SceneData>, updates: usize) -> (Vec<Record>, App) {
    let mut app = app(scene);
    let mut records = Vec::new();
    for _ in 0..updates {
        app.update();
        let clock = app.world().resource::<ShowcaseClock>().frame;
        let stats = app.world().resource::<ShowcaseStats>().clone();
        let mut query = app.world_mut().query::<(&Tracked, &Transform)>();
        let pod = query
            .iter(app.world())
            .next()
            .map(|(_, t)| t.translation.to_array())
            .expect("the tracked entity exists after the first update");
        records.push(Record {
            clock,
            effects: stats.effects_fired,
            decals: stats.decals_fired,
            changes: stats.weather_changes,
            pod,
        });
    }
    (records, app)
}

#[test]
fn the_clock_counts_fixed_steps_from_the_lead_in() {
    let (records, _) = run(scene(), 30);
    let last = records.last().expect("records");
    assert!(last.clock > 20 && last.clock <= 30, "clock {}", last.clock);
    assert!(
        records.windows(2).all(|w| w[1].clock - w[0].clock <= 1),
        "never more than one step per update"
    );
}

#[test]
fn spawns_fire_once_on_their_frames() {
    let scene = scene();
    let effect_frames: Vec<i64> = scene.spawns.effects.iter().map(|e| i64::from(e.frame)).collect();
    let decal_frames: Vec<i64> = scene.spawns.decals.iter().map(|d| i64::from(d.frame)).collect();
    let (records, _) = run(scene, 260);
    // The frames evaluated so far are those below the clock.
    for record in &records {
        let want_effects = effect_frames.iter().filter(|&&f| f < record.clock).count() as u32;
        let want_decals = decal_frames.iter().filter(|&&f| f < record.clock).count() as u32;
        assert_eq!(record.effects, want_effects, "effects at clock {}", record.clock);
        assert_eq!(record.decals, want_decals, "decals at clock {}", record.clock);
    }
    let last = records.last().expect("records");
    assert_eq!(
        (last.effects, last.decals),
        (3, 2),
        "everything fired exactly once"
    );
    assert_eq!(last.changes, 1, "the weather change applied once");
}

#[test]
fn the_pod_follows_the_track() {
    let scene = scene();
    let track = scene.track.clone().expect("track");
    let (records, _) = run(Arc::clone(&scene), 250);
    for record in &records {
        let evaluated = record.clock - 1;
        let pose = track.sample(evaluated);
        let ground = scene.map.sample_bilinear(pose.position.x, pose.position.z);
        let want = [pose.position.x, pose.position.y + ground, pose.position.z];
        for axis in 0..3 {
            assert!(
                (record.pod[axis] - want[axis]).abs() < 1e-3,
                "frame {evaluated} axis {axis}: {:?} vs {want:?}",
                record.pod
            );
        }
    }
    // It lands: the last record sits at the landing key, above the ground by the capsule's rest height.
    let end = records.last().expect("records").pod;
    assert!(end[1] < 40.0, "the pod has descended: {end:?}");
}

#[test]
fn two_runs_agree_record_for_record() {
    let (a, _) = run(scene(), 220);
    let (b, _) = run(scene(), 220);
    assert_eq!(a, b);
}

#[test]
fn effects_and_decals_reach_the_engine_state() {
    let (_, app) = run(scene(), 200);
    let world = app.world();
    let vfx = world.resource::<VfxScene>();
    assert!(vfx.library().index("explosion").is_some());
    assert!(
        vfx.live_instances() > 0,
        "the effects fired around frame 180 are alive on the vfx scene"
    );
    let decals = world.resource::<DecalPool>();
    assert!(decals.len() >= 2, "{} decals in the pool", decals.len());
    let weather = world.resource::<Weather>();
    assert!(weather.transitioning() || weather.target() != regia_vfx::WeatherState::FAIR);
}

#[test]
fn the_lead_in_starts_the_clock_early() {
    let mut data = load_scene(&example_dir()).expect("scene");
    data.manifest.lead_in_frames = 30;
    let mut app = app(Arc::new(data));
    app.update();
    let clock = app.world().resource::<ShowcaseClock>().frame;
    assert!(
        (-30..=-28).contains(&clock),
        "clock {clock} after one update with a 30-frame lead-in"
    );
}
