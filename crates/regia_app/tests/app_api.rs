// SPDX-License-Identifier: MPL-2.0
//! Public-behavior and error-path tests for `App`: plugin registration, exit handling, runners,
//! out-of-frame schedules, `Diagnostics` and the ambiguity report (sprint test-gap closure for
//! `regia_app`). Deterministic: manual time and counters only.

use std::num::NonZeroU8;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::prelude::*;
use regia_app::{DiagnosticsConfig, DiagnosticsPlugin, update_diagnostics};

const MS16: Duration = Duration::from_millis(16);

fn manual_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(MS16));
    app
}

type Events = Arc<Mutex<Vec<String>>>;

/// Logs `build:` / `finish:` / `cleanup:` + its name into a shared list.
struct Tracer {
    name: &'static str,
    events: Events,
}

impl Tracer {
    fn push(&self, phase: &str) {
        if let Ok(mut events) = self.events.lock() {
            events.push(format!("{phase}:{}", self.name));
        }
    }
}

impl Plugin for Tracer {
    fn build(&self, _app: &mut App) {
        self.push("build");
    }
    fn finish(&self, _app: &mut App) {
        self.push("finish");
    }
    fn cleanup(&self, _app: &mut App) {
        self.push("cleanup");
    }
    fn name(&self) -> &str {
        self.name
    }
}

fn tracer(name: &'static str, events: &Events) -> Tracer {
    Tracer {
        name,
        events: Arc::clone(events),
    }
}

fn events_of(events: &Events) -> Vec<String> {
    events.lock().map(|e| e.clone()).unwrap_or_default()
}

// ---- plugins ----

#[test]
fn a_tuple_of_eight_plugins_builds_in_order() {
    let events = Events::default();
    let mut app = App::new();
    app.add_plugins((
        tracer("a", &events),
        tracer("b", &events),
        tracer("c", &events),
        tracer("d", &events),
        tracer("e", &events),
        tracer("f", &events),
        tracer("g", &events),
        tracer("h", &events),
    ));
    let built: Vec<String> = events_of(&events);
    let expected: Vec<String> = ["a", "b", "c", "d", "e", "f", "g", "h"]
        .iter()
        .map(|n| format!("build:{n}"))
        .collect();
    assert_eq!(built, expected);
    for name in ["a", "h"] {
        assert!(app.has_plugin(name));
    }
    assert!(!app.has_plugin("i"));
}

#[test]
fn nested_tuples_flatten_in_order() {
    let events = Events::default();
    let mut app = App::new();
    app.add_plugins((tracer("a", &events), (tracer("b", &events), tracer("c", &events))));
    assert_eq!(events_of(&events), ["build:a", "build:b", "build:c"]);
}

#[test]
fn a_duplicate_name_is_skipped_without_building() {
    let events = Events::default();
    let mut app = App::new();
    app.add_plugins(tracer("same", &events));
    app.add_plugins(tracer("same", &events));
    app.add_plugins(tracer("other", &events));
    assert_eq!(events_of(&events), ["build:same", "build:other"]);
}

#[test]
fn finish_plugins_runs_finish_then_cleanup_once_per_plugin() {
    let events = Events::default();
    let mut app = App::new();
    app.add_plugins((tracer("a", &events), tracer("b", &events)));
    app.finish_plugins();
    app.finish_plugins();
    assert_eq!(
        events_of(&events),
        [
            "build:a",
            "build:b",
            "finish:a",
            "finish:b",
            "cleanup:a",
            "cleanup:b"
        ]
    );
}

#[test]
fn run_finishes_plugins_before_the_runner_gets_the_app() {
    let events = Events::default();
    let mut app = App::new();
    app.add_plugins(tracer("a", &events));
    let seen = Arc::clone(&events);
    app.set_runner(move |_app| {
        let at_runner = seen.lock().map(|e| e.clone()).unwrap_or_default();
        assert_eq!(at_runner, ["build:a", "finish:a", "cleanup:a"]);
        AppExit::Error(NonZeroU8::new(3).expect("3 is non-zero"))
    });
    assert_eq!(
        app.run(),
        AppExit::Error(NonZeroU8::new(3).expect("3 is non-zero"))
    );
}

#[test]
fn the_last_runner_wins() {
    let mut app = App::new();
    app.set_runner(|_| AppExit::Error(NonZeroU8::MIN));
    app.set_runner(|_| AppExit::Success);
    assert_eq!(app.run(), AppExit::Success);
}

// ---- exit ----

#[test]
fn should_exit_is_none_until_a_request_and_reports_the_error_code() {
    let mut app = App::new();
    app.update();
    assert_eq!(app.should_exit(), None);
    let code = NonZeroU8::new(42).expect("42 is non-zero");
    app.world_mut().write_message(AppExit::Error(code));
    assert_eq!(app.should_exit(), Some(AppExit::Error(code)));
    assert_eq!(app.should_exit(), None, "a request is reported once");
}

#[test]
fn an_error_exit_from_a_system_ends_the_headless_runner_with_its_code() {
    let code = NonZeroU8::new(7).expect("7 is non-zero");
    let mut app = App::new();
    app.add_systems(
        Update,
        move |time: Res<Time>, mut exit: MessageWriter<AppExit>| {
            if time.frame_count() == 2 {
                exit.write(AppExit::Error(code));
            }
        },
    );
    assert_eq!(app.run(), AppExit::Error(code));
}

#[test]
fn run_headless_stops_on_the_frame_that_requested_exit() {
    let mut app = App::new();
    app.add_systems(Update, |time: Res<Time>, mut exit: MessageWriter<AppExit>| {
        if time.frame_count() == 5 {
            exit.write(AppExit::Success);
        }
    });
    app.set_runner(|mut app| {
        let exit = loop {
            app.update();
            if let Some(exit) = app.should_exit() {
                break exit;
            }
        };
        assert_eq!(app.world().resource::<Time>().frame_count(), 5);
        exit
    });
    assert_eq!(app.run(), AppExit::Success);
}

// ---- schedules ----

#[test]
fn run_schedule_of_an_unknown_label_is_logged_and_ignored() {
    #[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct NeverAdded;
    let mut app = App::new();
    app.run_schedule(NeverAdded);
    app.update();
    assert_eq!(app.world().resource::<Time>().frame_count(), 1);
}

#[test]
fn lifecycle_and_level_schedules_run_only_when_asked() {
    #[derive(Resource, Default)]
    struct Hits {
        lifecycle: u32,
        loaded: u32,
        unloading: u32,
    }
    let mut app = manual_app();
    app.init_resource::<Hits>();
    app.add_systems(Lifecycle, |mut h: ResMut<Hits>| h.lifecycle += 1);
    app.add_systems(OnLevelLoaded, |mut h: ResMut<Hits>| h.loaded += 1);
    app.add_systems(OnLevelUnloading, |mut h: ResMut<Hits>| h.unloading += 1);
    app.update();
    app.update();
    let hits = app.world().resource::<Hits>();
    assert_eq!((hits.lifecycle, hits.loaded, hits.unloading), (0, 0, 0));
    app.run_lifecycle();
    app.run_schedule(OnLevelLoaded);
    app.run_schedule(OnLevelLoaded);
    app.run_schedule(OnLevelUnloading);
    let hits = app.world().resource::<Hits>();
    assert_eq!((hits.lifecycle, hits.loaded, hits.unloading), (1, 2, 1));
}

#[test]
fn startup_runs_once_and_before_the_first_update() {
    #[derive(Resource, Default)]
    struct Order(Vec<&'static str>);
    let mut app = manual_app();
    app.init_resource::<Order>();
    app.add_systems(Startup, |mut o: ResMut<Order>| o.0.push("startup"));
    app.add_systems(First, |mut o: ResMut<Order>| o.0.push("first"));
    for _ in 0..3 {
        app.update();
    }
    let order = &app.world().resource::<Order>().0;
    assert_eq!(order.iter().filter(|s| **s == "startup").count(), 1);
    assert_eq!(order[..2], ["startup", "first"]);
}

// ---- diagnostics ----

#[test]
fn diagnostics_window_is_bounded_and_counts_every_frame() {
    let mut app = manual_app();
    app.add_plugins(DiagnosticsPlugin {
        config: DiagnosticsConfig {
            window_frames: 4,
            log_interval: None,
        },
    });
    for _ in 0..10 {
        app.update();
    }
    let diagnostics = app.world().resource::<Diagnostics>();
    assert_eq!(diagnostics.frames, 10);
    assert_eq!(diagnostics.window().count(), 4);
    assert!(diagnostics.window().all(|d| d == MS16));
    assert_eq!(diagnostics.frame_time_avg, MS16);
    assert!((diagnostics.fps - 62.5).abs() < 1e-9);
    assert_eq!(diagnostics.frame_time_1pct_low, MS16);
}

fn archetype_entity_sum(world: &World) -> u32 {
    world
        .archetypes()
        .iter()
        .fold(0u32, |sum, archetype| sum.saturating_add(archetype.len()))
}

#[test]
fn diagnostics_entity_count_follows_spawns_and_despawns() {
    let mut app = manual_app();
    app.add_plugins(DiagnosticsPlugin::default());
    app.update();
    let base = app.world().resource::<Diagnostics>().entity_count;
    let spawned: Vec<_> = (0..5).map(|_| app.world_mut().spawn_empty().id()).collect();
    app.update();
    assert_eq!(app.world().resource::<Diagnostics>().entity_count, base + 5);
    assert_eq!(
        app.world().resource::<Diagnostics>().entity_count,
        archetype_entity_sum(app.world())
    );
    for entity in spawned {
        app.world_mut().despawn(entity);
    }
    app.update();
    assert_eq!(app.world().resource::<Diagnostics>().entity_count, base);
    assert_eq!(base, archetype_entity_sum(app.world()));

    // A few thousand entities, then all but one despawned: the index high-water mark stays
    // large, and the published count stays the live archetype sum (and `count_spawned`).
    #[derive(Component)]
    struct Marker;
    #[derive(Component)]
    struct Other;
    let many: Vec<_> = (0..4_000u32)
        .map(|i| {
            if i.is_multiple_of(2) {
                app.world_mut().spawn(Marker).id()
            } else {
                app.world_mut().spawn((Marker, Other)).id()
            }
        })
        .collect();
    app.update();
    assert_eq!(app.world().resource::<Diagnostics>().entity_count, base + 4_000);
    assert_eq!(
        app.world().resource::<Diagnostics>().entity_count,
        archetype_entity_sum(app.world())
    );
    assert_eq!(
        archetype_entity_sum(app.world()),
        app.world().entities().count_spawned()
    );
    for entity in many.into_iter().skip(1) {
        app.world_mut().despawn(entity);
    }
    app.update();
    let published = app.world().resource::<Diagnostics>().entity_count;
    let live = archetype_entity_sum(app.world());
    assert_eq!(published, base + 1);
    assert_eq!(published, live);
    assert_eq!(live, app.world().entities().count_spawned());
    assert!(
        u64::from(app.world().entities().len()) > u64::from(published) + 1_000,
        "despawn left no index high-water mark"
    );
}

#[test]
fn update_diagnostics_without_the_resource_is_harmless() {
    let mut app = manual_app();
    app.update();
    assert!(!app.world().contains_resource::<Diagnostics>());
    update_diagnostics(app.world_mut());
    assert!(!app.world().contains_resource::<Diagnostics>());
}

#[test]
fn diagnostics_window_of_zero_frames_keeps_one_sample() {
    let mut d = Diagnostics::default();
    d.record_frame(MS16, 0);
    d.record_frame(MS16 * 2, 0);
    assert_eq!(d.window().collect::<Vec<_>>(), [MS16 * 2]);
    assert_eq!(d.frames, 2);
}

#[test]
fn diagnostics_zero_deltas_count_as_frames_but_not_samples() {
    let mut d = Diagnostics::default();
    d.record_frame(Duration::ZERO, 10);
    assert_eq!(d.frames, 1);
    assert_eq!(d.window().count(), 0);
    assert_eq!(d.fps, 0.0);
    assert_eq!(d.frame_time_avg, Duration::ZERO);
}

#[test]
fn diagnostics_one_percent_low_averages_the_slowest_hundredth() {
    let mut d = Diagnostics::default();
    for _ in 0..199 {
        d.record_frame(Duration::from_millis(10), 1000);
    }
    d.record_frame(Duration::from_millis(30), 1000);
    d.record_frame(Duration::from_millis(50), 1000);
    // 201 samples: ceil(201 / 100) = 3 worst frames: 50, 30 and one 10 ms.
    let expected = Duration::from_millis(90) / 3;
    assert_eq!(d.frame_time_1pct_low, expected);
    assert!((d.fps_1pct_low - 1.0 / expected.as_secs_f64()).abs() < 1e-9);
}

#[test]
fn diagnostics_summary_mentions_allocation_only_when_known() {
    let mut d = Diagnostics::default();
    d.record_frame(Duration::from_millis(20), 10);
    d.entity_count = 12;
    d.draw_calls = 34;
    let plain = d.summary();
    assert!(plain.contains("50.0 fps"), "{plain}");
    assert!(plain.contains("12 entities"), "{plain}");
    assert!(plain.contains("34 draw calls"), "{plain}");
    assert!(!plain.contains("MiB"), "{plain}");
    d.allocated_bytes = Some(3 * 1024 * 1024);
    assert!(d.summary().contains("3.0 MiB allocated"), "{}", d.summary());
}

#[test]
fn diagnostics_config_defaults() {
    let config = DiagnosticsConfig::default();
    assert_eq!(config.window_frames, 300);
    assert_eq!(config.log_interval, None);
}

// ---- ambiguity report ----

#[test]
fn an_app_without_conflicts_reports_nothing() {
    let mut app = App::new();
    app.add_systems(Update, || {});
    let report = app.ambiguity_report();
    assert_eq!(report.total(), 0);
    assert!(report.ranked().is_empty());
    assert!(report.build_errors.is_empty(), "{:?}", report.build_errors);
    let md = report.to_markdown(5);
    assert!(md.starts_with("| schedule | systems | ambiguous pairs |"), "{md}");
}

#[test]
fn a_system_cycle_is_listed_as_a_build_error_instead_of_panicking() {
    fn a() {}
    fn b() {}
    let mut app = App::new();
    app.add_systems(Update, (a.before(b), b.before(a)));
    let report = app.ambiguity_report();
    assert!(!report.build_errors.is_empty(), "{report:?}");
    assert!(report.to_markdown(1).contains("build error"));
}

#[test]
fn ranked_pairs_are_sorted_by_score_and_markdown_honors_the_limit() {
    #[derive(Resource, Default)]
    struct Hot(u32);
    #[derive(Resource, Default)]
    struct Cold(u32);
    let mut app = App::new();
    app.init_resource::<Hot>().init_resource::<Cold>();
    // Read/write on `Cold` (score 1), two writers of `Hot` (score 3).
    app.add_systems(Update, |_: Res<Cold>| {});
    app.add_systems(Update, |mut c: ResMut<Cold>| c.0 += 1);
    app.add_systems(Update, |mut h: ResMut<Hot>| h.0 += 1);
    app.add_systems(Update, |mut h: ResMut<Hot>| h.0 += 1);
    let report = app.ambiguity_report();
    let scores: Vec<usize> = report.ranked().iter().map(|(_, p)| p.score()).collect();
    assert_eq!(scores, [3, 1]);
    let one = report.to_markdown(1);
    assert!(one.contains("score 3"), "{one}");
    assert!(!one.contains("score 1:"), "{one}");
    let none = report.to_markdown(0);
    assert!(!none.contains("<->"), "{none}");
}

// ---- plugin lifecycle edge cases (grok review 11-02) ----

#[test]
fn forgetting_an_owner_drops_its_plugin_objects_and_late_adds_are_finished() {
    let events = Events::default();
    let mut app = App::new();
    let game = TypeOwner::Game(21);
    app.with_owner(game, |app| {
        app.add_plugins(tracer("reloaded", &events));
    });
    app.finish_plugins();
    assert_eq!(
        events_of(&events),
        ["build:reloaded", "finish:reloaded", "cleanup:reloaded"]
    );
    app.forget_owner_plugins(game);
    app.with_owner(game, |app| {
        app.add_plugins(tracer("reloaded", &events));
    });
    // Added after `finish_plugins`: finished at once, and `finish_plugins` stays a no-op.
    assert_eq!(
        events_of(&events)[3..],
        ["build:reloaded", "finish:reloaded", "cleanup:reloaded"]
    );
    app.finish_plugins();
    assert_eq!(events_of(&events).len(), 6);
}

#[test]
fn a_plugin_added_from_cleanup_is_finished() {
    struct Parent(Events);
    impl Plugin for Parent {
        fn build(&self, _app: &mut App) {}
        fn cleanup(&self, app: &mut App) {
            let events = Arc::clone(&self.0);
            app.add_plugins(tracer("child", &events));
        }
        fn name(&self) -> &str {
            "parent"
        }
    }
    let events = Events::default();
    let mut app = App::new();
    app.add_plugins(Parent(Arc::clone(&events)));
    app.finish_plugins();
    assert_eq!(
        events_of(&events),
        ["build:child", "finish:child", "cleanup:child"]
    );
}
