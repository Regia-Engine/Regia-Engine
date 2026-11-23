// SPDX-License-Identifier: MPL-2.0
//! Ambient life in an app (ADR-3024): off until a project turns it on, the scene's time and
//! weather set the counts, the tier gates it, birds fly from an anchor (or near the eye when the
//! host places none), and an entity can follow a bird (an audio call's emitter).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{QualityTier, RenderSettings, TimeOfDay};
use regia_scene::Transform;
use regia_scene::level::{LevelSettings, LevelTimeOfDay};
use regia_vfx::ambient::{AmbientState, AutoAmbientBirds, BirdFlockSim, ambient_tier};
use regia_vfx::weather::WeatherPlugin;
use regia_vfx::{
    AmbientBirds, AmbientLifeHandle, AmbientLifePlugin, AmbientLifeReport, AmbientLifeSettings,
    FollowAmbientBird, Season, VfxCamera, VfxView, Weather, WeatherState,
};

const STEP_MS: u64 = 50;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(STEP_MS),
    ));
    app.add_plugins((regia_app::MinimalPlugins, WeatherPlugin, AmbientLifePlugin));
    app.insert_resource(TimeOfDay {
        hour: 15.0,
        day_of_year: 200,
        latitude_deg: 45.0,
    });
    app
}

fn on(app: &mut App) {
    app.insert_resource(AmbientLifeSettings {
        enabled: true,
        ..AmbientLifeSettings::default()
    });
}

fn state(app: &App) -> AmbientState {
    app.world()
        .resource::<AmbientLifeHandle>()
        .state
        .lock()
        .map(|s| s.clone())
        .expect("state lock")
}

#[test]
fn off_until_turned_on() {
    let mut app = app();
    for _ in 0..5 {
        app.update();
    }
    assert!(!app.world().resource::<AmbientLifeSettings>().enabled);
    assert!(
        app.world().get_resource::<AmbientLifeReport>().is_none(),
        "no work, no report"
    );
    assert!(!state(&app).active);
    let flocks = app.world_mut().query::<&AmbientBirds>().iter(app.world()).count();
    assert_eq!(flocks, 0, "no birds placed while off");
}

#[test]
fn a_summer_afternoon_turns_on_and_a_storm_quiets_it() {
    let mut app = app();
    on(&mut app);
    for _ in 0..10 {
        app.update();
    }
    let report = *app.world().resource::<AmbientLifeReport>();
    assert!(report.active);
    assert_eq!(report.season, Season::Summer);
    assert_eq!(report.quality, QualityTier::High);
    assert!(
        report.counts.motes > 100.0 && report.counts.swarm_members > 10.0,
        "{report:?}"
    );
    assert!(
        report.counts.butterflies > 1.0 && report.counts.leaves > 1.0,
        "{report:?}"
    );
    assert_eq!(report.counts.fireflies, 0.0, "no fireflies in the afternoon");
    assert_eq!(report.flocks, 2, "a flock and a soaring bird near the eye");
    assert!(report.birds_drawn > 0);
    let s = state(&app);
    assert!(s.active && s.bird_count == report.birds_drawn);
    assert!(s.integrator.clock_s > 0.0, "the drifts run on game time");

    app.insert_resource(Weather::new(WeatherState::STORM));
    for _ in 0..10 {
        app.update();
    }
    let storm = *app.world().resource::<AmbientLifeReport>();
    assert!(storm.conditions.rain_mm_per_h > 1.0, "{storm:?}");
    assert_eq!(storm.counts.birds, 0.0);
    assert_eq!(storm.birds_drawn, 0);
    assert_eq!((storm.counts.motes, storm.counts.butterflies), (0.0, 0.0));
}

#[test]
fn the_low_tier_draws_nothing() {
    let mut app = app();
    on(&mut app);
    app.insert_resource(RenderSettings {
        profile: QualityTier::Low.profile(),
        ..RenderSettings::default()
    });
    for _ in 0..5 {
        app.update();
    }
    let report = *app.world().resource::<AmbientLifeReport>();
    assert_eq!(report.quality, QualityTier::Low);
    assert!(!report.active && report.budget.is_off());
    assert!(state(&app).frame_at([0.0; 3]).is_empty());
}

#[test]
fn turning_it_off_stops_the_draw() {
    let mut app = app();
    on(&mut app);
    for _ in 0..5 {
        app.update();
    }
    assert!(state(&app).active);
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = false;
    for _ in 0..3 {
        app.update();
    }
    assert!(!app.world().resource::<AmbientLifeReport>().active);
    assert!(state(&app).frame_at([0.0; 3]).is_empty());
}

#[test]
fn a_host_flock_replaces_the_automatic_birds_and_a_follower_tracks_its_bird() {
    let mut app = app();
    on(&mut app);
    for _ in 0..3 {
        app.update();
    }
    let auto = app
        .world_mut()
        .query::<&AutoAmbientBirds>()
        .iter(app.world())
        .count();
    assert_eq!(auto, 2);
    let anchor = Vec3::new(40.0, 5.0, -70.0);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds {
                call_sounds: vec!["bird_call_a".into()],
                seed: 9,
                ..AmbientBirds::flock()
            },
            Transform::from_translation(anchor),
        ))
        .id();
    let follower = app
        .world_mut()
        .spawn((Transform::IDENTITY, FollowAmbientBird { flock, bird: 2 }))
        .id();
    for _ in 0..20 {
        app.update();
    }
    let auto = app
        .world_mut()
        .query::<&AutoAmbientBirds>()
        .iter(app.world())
        .count();
    assert_eq!(auto, 0, "the host's own flock replaces the automatic birds");
    let report = *app.world().resource::<AmbientLifeReport>();
    assert_eq!(report.flocks, 1);
    let sim = app.world().get::<BirdFlockSim>(flock).expect("the flight");
    assert!(sim.steps() > 0);
    let bird = sim.position(2).expect("bird 2");
    let at = app
        .world()
        .get::<Transform>(follower)
        .expect("follower")
        .translation;
    assert!(at.distance(bird) < 1.0e-4, "{at} vs {bird}");
    assert!((at - anchor).length() < sim.config().radius_m * 2.0 + sim.config().height_m * 2.0);
    // The flock goes; the follower lets go.
    app.world_mut().entity_mut(flock).despawn();
    app.update();
    assert!(app.world().get::<FollowAmbientBird>(follower).is_none());
}

#[test]
fn the_same_steps_fly_the_same_birds() {
    let run = || {
        let mut app = app();
        on(&mut app);
        app.world_mut().spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -50.0)),
        ));
        for _ in 0..40 {
            app.update();
        }
        let s = state(&app);
        (s.bird_count, s.birds)
    };
    assert_eq!(run(), run());
}

#[test]
fn the_automatic_birds_follow_a_travelling_camera() {
    let mut app = app();
    on(&mut app);
    // The camera `VfxPlugin` keeps (its runtime writes the rendered view here).
    app.insert_resource(regia_vfx::VfxView::default());
    for _ in 0..3 {
        app.update();
    }
    let anchors = |app: &mut App| -> Vec<Vec3> {
        let mut query = app.world_mut().query::<(&BirdFlockSim, &AutoAmbientBirds)>();
        query.iter(app.world()).map(|(sim, _)| sim.anchor()).collect()
    };
    let start = anchors(&mut app);
    assert_eq!(start.len(), 2);
    let far = Vec3::new(2_000.0, 10.0, -3_000.0);
    app.world_mut().resource_mut::<regia_vfx::VfxView>().camera.eye = far;
    for _ in 0..3 {
        app.update();
    }
    for anchor in anchors(&mut app) {
        let away = anchor - far;
        let d = away.x.hypot(away.z);
        assert!(d < 200.0, "an automatic flock stayed {d} m behind");
    }
}

/// The automatic birds appear in the shot, wherever the camera looks: ahead of the view, and over
/// a forest's crowns.
#[test]
fn the_automatic_birds_start_ahead_of_the_view() {
    for ahead in [Vec3::X, Vec3::NEG_Z, Vec3::new(-0.6, 0.0, 0.8)] {
        let mut app = app();
        on(&mut app);
        let eye = Vec3::new(120.0, 300.0, -50.0);
        let mut view = regia_vfx::VfxView::default();
        view.camera.eye = eye;
        // Looking up a little: the height of the look does not move them.
        view.camera.target = eye + ahead * 30.0 + Vec3::Y * 8.0;
        app.insert_resource(view);
        for _ in 0..3 {
            app.update();
        }
        let mut query = app.world_mut().query::<(&BirdFlockSim, &AutoAmbientBirds)>();
        let anchors: Vec<Vec3> = query.iter(app.world()).map(|(sim, _)| sim.anchor()).collect();
        assert_eq!(anchors.len(), 2);
        for anchor in anchors {
            let away = anchor - eye;
            let along = away.dot(ahead);
            assert!(along > 50.0, "{along} m ahead of a view along {ahead}");
            assert!(
                along > 2.0 * away.dot(Vec3::new(-ahead.z, 0.0, ahead.x)).abs(),
                "{away} is off the view"
            );
        }
        let mut flocks = app.world_mut().query::<(&AmbientBirds, &AutoAmbientBirds)>();
        for (birds, _) in flocks.iter(app.world()) {
            assert!(birds.height_m >= 30.0, "under the crowns: {}", birds.height_m);
        }
    }
}

/// The layer wishes for the sun's view while motes or swarms fly, and stops when they do not.
#[test]
fn the_sun_view_is_wanted_while_motes_or_swarms_fly() {
    let mut app = app();
    app.update();
    assert!(!state(&app).wants_sun_view, "off");
    on(&mut app);
    app.update();
    let now = state(&app);
    assert!(now.counts.motes > 0.0 && now.wants_sun_view, "{:?}", now.counts);
    app.world_mut().resource_mut::<AmbientLifeSettings>().motes = false;
    app.world_mut().resource_mut::<AmbientLifeSettings>().insects = false;
    app.update();
    assert!(!state(&app).wants_sun_view, "nothing to shade");
    app.world_mut().resource_mut::<AmbientLifeSettings>().motes = true;
    app.update();
    assert!(state(&app).wants_sun_view, "back with the motes");
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = false;
    app.update();
    assert!(!state(&app).wants_sun_view, "turned off");
}

/// The clocks a render-out job restarts (ADR-1881): `ran` frames, then the restart, then 40 more.
fn after_a_restart(ran: u32) -> (AmbientState, Vec<u32>) {
    let mut app = app();
    on(&mut app);
    for _ in 0..ran {
        app.update();
    }
    app.world_mut()
        .resource_mut::<Time>()
        .set_clocks(std::time::Duration::ZERO, std::time::Duration::ZERO);
    for _ in 0..40 {
        app.update();
    }
    let now = state(&app);
    assert_eq!(now.restarts, 1, "one restart");
    let birds = now.birds[..now.bird_count as usize]
        .iter()
        .flat_map(|bird| bird.position_size)
        .map(f32::to_bits)
        .collect();
    (now, birds)
}

/// A job's frames do not depend on how long the app ran before it: from the clock's restart on, the
/// animation clock, the drifts and the birds are the same bits whatever came first.
#[test]
fn a_restarted_clock_restarts_the_life() {
    let (short, short_birds) = after_a_restart(10);
    let (long, long_birds) = after_a_restart(97);
    assert!(!short_birds.is_empty(), "birds fly");
    assert_eq!(short.integrator, long.integrator);
    assert_eq!(short_birds, long_birds);
    assert!(short.integrator.clock_s > 0.0, "it runs on");
}

/// A clock that only runs on restarts nothing.
#[test]
fn a_running_clock_restarts_nothing() {
    let mut app = app();
    on(&mut app);
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(state(&app).restarts, 0);
}

/// A scene whose sky runs on the level's sun clock has no `TimeOfDay` resource: the time and the
/// season still come from the clock the sky draws, and a host's resource still wins, as in the
/// renderer.
#[test]
fn the_levels_sun_clock_sets_the_time_and_the_season() {
    let mut app = app();
    app.world_mut().remove_resource::<TimeOfDay>();
    let mut level = LevelSettings::default();
    level.atmosphere.time_of_day = Some(LevelTimeOfDay {
        hour: 8.0,
        day_of_year: 300,
        latitude_deg: 46.0,
    });
    app.insert_resource(level);
    on(&mut app);
    app.update();
    let drawn = |app: &App| {
        let report = app.world().resource::<AmbientLifeReport>();
        (report.conditions.sun_elevation_sin, report.season)
    };
    let of_level = regia_render::sun_position(&TimeOfDay {
        hour: 8.0,
        day_of_year: 300,
        latitude_deg: 46.0,
    });
    assert_eq!(drawn(&app), (of_level.direction[1], Season::Autumn));
    let host = TimeOfDay {
        hour: 12.0,
        day_of_year: 180,
        latitude_deg: 46.0,
    };
    app.insert_resource(host);
    app.update();
    assert_eq!(
        drawn(&app),
        (regia_render::sun_position(&host).direction[1], Season::Summer)
    );
}

/// A flock on a moving entity (a boat, a host's roaming spot) keeps flying: the anchor carries the
/// flight along. A restart per frame would hold every bird at the start of its path.
#[test]
fn a_moving_anchor_carries_the_flight_along() {
    let mut app = app();
    on(&mut app);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
    let (steps_before, first_before) = (sim.steps(), sim.position(0).expect("bird") - sim.anchor());
    for frame in 1..=60 {
        let mut transform = app.world_mut().get_mut::<Transform>(flock).expect("anchor");
        transform.translation.x = frame as f32;
        app.update();
    }
    let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
    assert_eq!(sim.anchor().x, 60.0, "the flight is at the anchor");
    assert!(
        sim.steps() >= steps_before + 150,
        "it kept flying: {} steps after {steps_before}",
        sim.steps()
    );
    let first = sim.position(0).expect("bird") - sim.anchor();
    assert!(
        first.distance(first_before) > 5.0,
        "the bird moved around its anchor: {first_before} to {first}"
    );
    assert!(
        first.length() < sim.config().radius_m + sim.config().height_m * 1.25 + 20.0,
        "and stayed with it: {first}"
    );
}

/// A changed flock still restarts its flight.
#[test]
fn a_changed_flock_restarts_its_flight() {
    let mut app = app();
    on(&mut app);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ))
        .id();
    for _ in 0..20 {
        app.update();
    }
    let before = app.world().get::<BirdFlockSim>(flock).expect("flight").steps();
    app.world_mut()
        .get_mut::<AmbientBirds>(flock)
        .expect("flock")
        .count = 3;
    app.update();
    let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
    assert_eq!(sim.len(), 3);
    assert!(sim.steps() < before, "{} after {before}", sim.steps());
}

/// A tier stays itself with the levers that move on it: the detector's terrain virtual texture (a
/// Low tier on a 6 GB card), features no preset turns on, a player's render scale or shadows, and a
/// host's own CPU memory budget (ADR-7891: it changes no pixel, as in `RenderSettings::preset`).
#[test]
fn a_tier_with_its_levers_moved_is_still_that_tier() {
    type Lever = dyn Fn(&mut regia_render::RenderProfile);
    for tier in QualityTier::ALL {
        let settings = |edit: &Lever| {
            let mut profile = tier.profile();
            edit(&mut profile);
            RenderSettings {
                profile,
                ..RenderSettings::default()
            }
        };
        let levers: [(&str, &Lever); 6] = [
            ("as shipped", &|_| {}),
            ("terrain virtual texture", &|p| {
                p.terrain_budget.virtual_texture = true;
            }),
            ("frame pipelining", &|p| {
                p.frame_pipelining = regia_render::FramePipelining::ON;
            }),
            ("player's render scale", &|p| p.render_scale = 0.5),
            ("player's shadows", &|p| {
                p.csm_resolution /= 2;
                p.local_shadow_atlas_size = 0;
                p.local_shadow_max_lights = 0;
            }),
            ("host memory budget", &|p| {
                p.host_memory = regia_render::host_memory::HostMemoryBudget {
                    cpu_mesh_bytes: 64 << 20,
                    cpu_texture_bytes: 32 << 20,
                };
            }),
        ];
        for (name, edit) in levers {
            assert_eq!(ambient_tier(Some(&settings(edit))), tier, "{tier:?} with {name}");
        }
    }
}

/// The promise a card's detected lever broke: Desktop Low on a 6 GB card draws nothing.
#[test]
fn the_low_tier_on_a_large_card_draws_nothing() {
    let mut app = app();
    on(&mut app);
    let mut profile = QualityTier::Low.profile();
    profile.terrain_budget.virtual_texture = true;
    app.insert_resource(RenderSettings {
        profile,
        ..RenderSettings::default()
    });
    for _ in 0..5 {
        app.update();
    }
    let report = *app.world().resource::<AmbientLifeReport>();
    assert_eq!(report.quality, QualityTier::Low);
    assert!(!report.active && report.budget.is_off());
    assert!(state(&app).frame_at([0.0; 3]).is_empty());
}

/// A tier changed while life is off counts when it comes back.
#[test]
fn a_tier_changed_while_off_counts_when_it_comes_back() {
    let mut app = app();
    on(&mut app);
    app.insert_resource(RenderSettings {
        profile: QualityTier::High.profile(),
        ..RenderSettings::default()
    });
    for _ in 0..3 {
        app.update();
    }
    assert!(state(&app).active);
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = false;
    for _ in 0..3 {
        app.update();
    }
    app.world_mut().resource_mut::<RenderSettings>().profile = QualityTier::Low.profile();
    for _ in 0..3 {
        app.update();
    }
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = true;
    app.update();
    let report = *app.world().resource::<AmbientLifeReport>();
    assert_eq!(report.quality, QualityTier::Low);
    assert!(!report.active);
}

fn auto_flights(app: &mut App) -> Vec<(u32, Vec3)> {
    let mut query = app
        .world_mut()
        .query_filtered::<(&AmbientBirds, &BirdFlockSim), With<AutoAmbientBirds>>();
    let mut flights: Vec<_> = query
        .iter(app.world())
        .map(|(config, sim)| {
            assert_eq!(config.seed, sim.config().seed, "it flies the seed it shows");
            (config.seed, sim.anchor())
        })
        .collect();
    flights.sort_by_key(|(seed, _)| *seed);
    flights
}

/// A new seed reaches the automatic birds, as the book says of every flight.
#[test]
fn a_new_seed_reaches_the_automatic_birds() {
    let mut app = app();
    on(&mut app);
    for _ in 0..6 {
        app.update();
    }
    let seeds: Vec<u32> = auto_flights(&mut app).iter().map(|(seed, _)| *seed).collect();
    assert_eq!(seeds, [0, 1]);
    app.world_mut().resource_mut::<AmbientLifeSettings>().seed = 7;
    app.update();
    let seeds: Vec<u32> = auto_flights(&mut app).iter().map(|(seed, _)| *seed).collect();
    assert_eq!(seeds, [7, 8]);
}

/// A flock changed while life is off flies as changed when it comes back: the change is not lost.
#[test]
fn a_flock_changed_while_off_counts_when_it_comes_back() {
    let mut app = app();
    on(&mut app);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ))
        .id();
    for _ in 0..5 {
        app.update();
    }
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = false;
    for _ in 0..2 {
        app.update();
    }
    app.world_mut()
        .get_mut::<AmbientBirds>(flock)
        .expect("flock")
        .count = 3;
    for _ in 0..2 {
        app.update();
    }
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = true;
    for _ in 0..2 {
        app.update();
    }
    assert_eq!(app.world().get::<BirdFlockSim>(flock).expect("flight").len(), 3);
}

/// Birds not drawn are not visible: an audio call cannot bind to one, and the report says so.
#[test]
fn birds_not_drawn_are_not_visible() {
    for turn_off in [
        (|s: &mut AmbientLifeSettings| s.birds = false) as fn(&mut AmbientLifeSettings),
        |s| s.enabled = false,
    ] {
        let mut app = app();
        on(&mut app);
        let flock = app
            .world_mut()
            .spawn((
                AmbientBirds::flock(),
                Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
            ))
            .id();
        for _ in 0..5 {
            app.update();
        }
        let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
        assert!(sim.visible() > 0.0);
        turn_off(&mut app.world_mut().resource_mut::<AmbientLifeSettings>());
        app.update();
        let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
        assert_eq!(sim.visible(), 0.0);
        assert!(sim.nearest_visible(Vec3::ZERO, sim.visible()).is_none());
        let report = *app.world().resource::<AmbientLifeReport>();
        assert_eq!((report.flocks, report.birds_drawn), (0, 0));
    }
}

/// Turning the automatic birds off takes the ones already flying away.
#[test]
fn turning_the_automatic_birds_off_removes_them() {
    let mut app = app();
    on(&mut app);
    for _ in 0..6 {
        app.update();
    }
    assert_eq!(auto_flights(&mut app).len(), 2);
    assert!(state(&app).bird_count > 0);
    app.world_mut().resource_mut::<AmbientLifeSettings>().auto_birds = false;
    for _ in 0..2 {
        app.update();
    }
    assert!(auto_flights(&mut app).is_empty());
    assert_eq!(state(&app).bird_count, 0);
}

/// A paused weather holds the sky; the creatures stop only with the game clock, all of them alike.
#[test]
fn a_paused_weather_holds_the_sky_not_the_creatures() {
    let mut app = app();
    on(&mut app);
    let mut weather = Weather::new(WeatherState::CLEAR);
    weather.paused = true;
    app.insert_resource(weather);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ))
        .id();
    for _ in 0..20 {
        app.update();
    }
    assert!(state(&app).integrator.clock_s > 0.5, "the creatures' clock runs");
    assert!(app.world().get::<BirdFlockSim>(flock).expect("flight").steps() > 30);
}

/// A clock that restarted while life was off still restarts it when it comes back, however far the
/// new clock has run by then.
#[test]
fn a_clock_restarted_while_off_counts_when_it_comes_back() {
    let mut app = app();
    on(&mut app);
    for _ in 0..10 {
        app.update();
    }
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = false;
    for _ in 0..2 {
        app.update();
    }
    app.world_mut()
        .resource_mut::<Time>()
        .set_clocks(std::time::Duration::ZERO, std::time::Duration::ZERO);
    for _ in 0..30 {
        app.update();
    }
    app.world_mut().resource_mut::<AmbientLifeSettings>().enabled = true;
    app.update();
    assert_eq!(state(&app).restarts, 1);
}

fn view_from(eye: Vec3, target: Vec3) -> VfxView {
    VfxView {
        camera: VfxCamera::looking_at(eye, target),
        ..VfxView::default()
    }
}

/// What a job delivers does not depend on where the camera was before it: [`VfxView`] is the last
/// drawn frame's camera, and the job's first frame has not drawn yet.
#[test]
fn a_jobs_automatic_birds_take_the_jobs_camera() {
    let job_view = || view_from(Vec3::new(100.0, 2.0, 0.0), Vec3::new(100.0, 2.0, -10.0));
    let run = |before: VfxView| {
        let mut app = app();
        on(&mut app);
        app.insert_resource(before);
        for _ in 0..10 {
            app.update();
        }
        app.world_mut()
            .resource_mut::<Time>()
            .set_clocks(std::time::Duration::ZERO, std::time::Duration::ZERO);
        // The job's first frame: the view is still the camera before the job.
        app.update();
        assert_eq!(
            state(&app).bird_count,
            0,
            "not drawn from a camera the job does not have"
        );
        app.insert_resource(job_view());
        for _ in 0..30 {
            app.update();
        }
        let now = state(&app);
        let birds: Vec<u32> = now.birds[..now.bird_count as usize]
            .iter()
            .flat_map(|bird| bird.position_size)
            .map(f32::to_bits)
            .collect();
        (auto_flights(&mut app), birds)
    };
    let (flights_a, birds_a) = run(view_from(Vec3::new(0.0, 2.0, 0.0), Vec3::new(10.0, 2.0, 0.0)));
    let (flights_b, birds_b) = run(view_from(Vec3::new(50.0, 40.0, 90.0), Vec3::new(0.0, 0.0, 0.0)));
    assert!(!birds_a.is_empty(), "birds fly");
    assert_eq!(flights_a, flights_b);
    assert_eq!(birds_a, birds_b);
    let flock = flights_a[0].1;
    assert!(
        flock.z < -30.0 && flock.x > 50.0,
        "ahead of the job's camera: {flock}"
    );
}

/// At start-up the view is its default, not the scene's camera: the automatic birds wait for it, even
/// when the scene's camera is near that default.
#[test]
fn the_automatic_birds_do_not_start_from_the_default_view() {
    let mut app = app();
    on(&mut app);
    app.insert_resource(VfxView::default());
    app.update();
    // The first drawn frame's camera, looking the other way from the default's.
    app.insert_resource(view_from(Vec3::new(0.0, 2.0, 6.0), Vec3::new(0.0, 2.0, 16.0)));
    for _ in 0..4 {
        app.update();
    }
    let flights = auto_flights(&mut app);
    assert_eq!(flights.len(), 2);
    assert!(flights[0].1.z > 50.0, "ahead of the view: {}", flights[0].1);
}

/// A host's NaN draws no birds, as it draws no motes.
#[test]
fn a_nan_density_draws_no_birds() {
    let mut app = app();
    app.insert_resource(AmbientLifeSettings {
        enabled: true,
        density_scale: f32::NAN,
        ..AmbientLifeSettings::default()
    });
    for _ in 0..6 {
        app.update();
    }
    assert_eq!(state(&app).bird_count, 0);
}

/// A follower does not stay on the frozen bird of a flock whose component was removed.
#[test]
fn removing_a_flocks_component_drops_its_followers() {
    let mut app = app();
    on(&mut app);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    let follower = app
        .world_mut()
        .spawn((Transform::IDENTITY, FollowAmbientBird { flock, bird: 2 }))
        .id();
    app.update();
    app.world_mut().entity_mut(flock).remove::<AmbientBirds>();
    for _ in 0..2 {
        app.update();
    }
    assert!(app.world().get::<FollowAmbientBird>(follower).is_none());
}

/// What the systems cost the CPU per frame at High and at Ultra's most birds, against the same app
/// with ambient life off, interleaved (printed, not asserted):
/// `cargo test -p regia_vfx --test ambient_life bench_ambient_cpu_cost -- --ignored --nocapture`
#[test]
#[ignore = "a measurement"]
fn bench_ambient_cpu_cost() {
    const FRAMES: usize = 400;
    let build = |enabled: bool, birds: u32| {
        let mut app = app();
        app.insert_resource(AmbientLifeSettings {
            enabled,
            auto_birds: false,
            ..AmbientLifeSettings::default()
        });
        app.world_mut().spawn((
            AmbientBirds {
                count: birds,
                ..AmbientBirds::flock()
            },
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ));
        for _ in 0..30 {
            app.update();
        }
        app
    };
    for birds in [32, 64] {
        let mut off = build(false, birds);
        let mut on = build(true, birds);
        let mut times = [Vec::with_capacity(FRAMES), Vec::with_capacity(FRAMES)];
        for frame in 0..FRAMES {
            // Interleaved, and either one first, so neither takes all of a load spike.
            for pick in [frame % 2, (frame + 1) % 2] {
                let app = if pick == 0 { &mut off } else { &mut on };
                let start = std::time::Instant::now();
                app.update();
                times[pick].push(start.elapsed().as_secs_f64() * 1e6);
            }
        }
        let median = |values: &mut Vec<f64>| {
            values.sort_by(f64::total_cmp);
            values[values.len() / 2]
        };
        let (a, b) = (median(&mut times[0].clone()), median(&mut times[1].clone()));
        println!(
            "{birds} birds: off {a:.1} us, on {b:.1} us, delta {:.1} us",
            b - a
        );
    }
}

/// A host flock whose anchor is hidden (`InheritedVisibility(false)`: its own or an ancestor's
/// `Visibility::Hidden`, or a parked layer member) flies no birds and is not visible to a call,
/// yet still keeps the automatic birds away; shown again, it draws (ADR-7200, ADR-7588). This app
/// has no visibility propagation, so the test sets the resolved value itself.
#[test]
fn a_hidden_host_flock_draws_no_birds() {
    use regia_scene::components::InheritedVisibility;
    let mut app = app();
    on(&mut app);
    let flock = app
        .world_mut()
        .spawn((
            AmbientBirds::flock(),
            Transform::from_translation(Vec3::new(0.0, 0.0, -40.0)),
        ))
        .id();
    for _ in 0..5 {
        app.update();
    }
    let report = *app.world().resource::<AmbientLifeReport>();
    assert!(report.birds_drawn > 0, "shown: {report:?}");
    assert!(app.world().get::<BirdFlockSim>(flock).expect("flight").visible() > 0.0);

    app.world_mut()
        .entity_mut(flock)
        .insert(InheritedVisibility(false));
    app.update();
    let report = *app.world().resource::<AmbientLifeReport>();
    assert_eq!((report.flocks, report.birds_drawn), (0, 0), "hidden: {report:?}");
    assert_eq!(state(&app).bird_count, 0);
    let sim = app.world().get::<BirdFlockSim>(flock).expect("flight");
    assert_eq!(sim.visible(), 0.0, "not drawn, so no call binds to it");
    let auto = app
        .world_mut()
        .query::<&AutoAmbientBirds>()
        .iter(app.world())
        .count();
    assert_eq!(auto, 0, "a hidden host flock is still the host's own");

    app.world_mut()
        .entity_mut(flock)
        .insert(InheritedVisibility(true));
    app.update();
    let report = *app.world().resource::<AmbientLifeReport>();
    assert_eq!(report.flocks, 1);
    assert!(report.birds_drawn > 0, "shown again: {report:?}");
}
