// SPDX-License-Identifier: MPL-2.0
use std::time::Duration;

use regia::physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia::telemetry::profile_capture::CaptureNode;
use regia_app::prelude::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_scene::Transform;

use super::*;
use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};

fn node(name: &str, inclusive: f64, children: Vec<CaptureNode>) -> CaptureNode {
    let child_sum: f64 = children.iter().map(|c| c.inclusive_ms).sum();
    CaptureNode {
        name: name.into(),
        inclusive_ms: inclusive,
        self_ms: inclusive - child_sum,
        calls: 1.0,
        children,
    }
}

fn capture(frame_ms: f64) -> ProfileCapture {
    ProfileCapture {
        format_version: 1,
        frames: 1,
        frame_ms,
        cpu: vec![
            node("render", 6.0, vec![node("forward", 3.0, vec![])]),
            node("update", 2.0, vec![]),
        ],
        truncated: 0,
        gpu: Vec::new(),
        folded: String::new(),
        timeline: Vec::new(),
        timeline_truncated: 0,
    }
}

fn handler(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(schema, _)| schema.name == name)
        .map(|(_, h)| h)
        .unwrap_or_else(|| panic!("{name} is registered"))
}

fn code(err: &RpcError) -> Option<String> {
    err.data
        .as_ref()
        .and_then(|d| d["code"].as_str().or_else(|| d["reason"].as_str()))
        .map(str::to_owned)
}

/// P-304 acceptance: in percent mode a frame's top-level spans and the time outside them sum to
/// 100 % of the frame; a frame whose spans cover it whole has no outside row.
#[test]
fn percent_mode_sums_to_100_for_a_frame() {
    let shares = frame_shares(&capture(10.0));
    let names: Vec<&str> = shares.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["render", "update", OUTSIDE_SPANS]);
    let total: f64 = shares.iter().map(|s| s.percent).sum();
    assert!((total - 100.0).abs() < 1e-9, "{shares:?}");
    assert!((shares[0].percent - 60.0).abs() < 1e-9);
    assert!((shares[2].ms - 2.0).abs() < 1e-9, "the 2 ms outside every span");
    // Spans that cover the frame: no outside row, still 100.
    let whole = frame_shares(&capture(8.0));
    assert_eq!(whole.len(), 2);
    assert!((whole.iter().map(|s| s.percent).sum::<f64>() - 100.0).abs() < 1e-9);
    // Rounded roots larger than the frame divide by their sum (never over 100).
    let over = frame_shares(&capture(7.5));
    assert!((over.iter().map(|s| s.percent).sum::<f64>() - 100.0).abs() < 1e-9);
    assert_eq!(time_text(2.0, 8.0, true), "25.0 %");
    assert_eq!(time_text(2.0, 8.0, false), "2.000 ms");
    assert_eq!(percent_of(1.0, 0.0), 0.0);
}

/// The percent toggle's agent twin answers the shares of the live frame (summing to 100) and sets
/// the mode the panel draws with; a non-boolean is refused.
#[test]
fn the_units_method_sets_the_mode_and_answers_shares_that_sum_to_100() {
    let profiler = regia_core::profiling::FrameProfiler::new();
    profiler.set_flame_enabled(true);
    let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
        tracing_subscriber::registry(),
        profiler.clone(),
    ));
    tracing::dispatcher::with_default(&dispatch, || {
        for _ in 0..3 {
            {
                let _a = tracing::info_span!("units_outer").entered();
                let _b = tracing::info_span!("units_inner").entered();
            }
            profiler.end_frame();
        }
    });
    let mut world = World::new();
    world.insert_resource(regia::Profiler(profiler));
    let units = handler("editor.profiler_units");
    let out = units(&mut world, &json!({ "percent": true })).expect("set");
    assert_eq!(out["percent"], true);
    assert!(percent_mode(&world));
    let total = out["total_percent"].as_f64().expect("total");
    assert!((total - 100.0).abs() < 1e-6, "{out:#}");
    let err = units(&mut world, &json!({ "percent": "yes" })).expect_err("refused");
    assert_eq!(code(&err).as_deref(), Some("profiler_bad_percent"));
    let out = units(&mut world, &json!({})).expect("read");
    assert_eq!(out["percent"], true, "absent keeps the mode");
}

/// The "% of frame" checkbox is reached with Tab and toggled with Space (no pointer).
#[test]
fn the_percent_toggle_is_operable_from_the_keyboard() {
    let mut world = World::new();
    let ctx = egui::Context::default();
    let size = [600.0, 200.0];
    let mut draw = |ui: &mut egui::Ui| {
        let _ = percent_toggle(ui, &mut world);
    };
    frame(&ctx, size, vec![], &mut draw);
    let update = frame(&ctx, size, vec![], &mut draw);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    tab_to(&ctx, size, "% of frame", false, 5, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    assert!(percent_mode(&world), "Space turned percent mode on");
}

fn physics_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins((regia_scene::ScenePlugin, PhysicsPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    app.update();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));
    for i in 0..3 {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32 * 2.0, 1.0 + i as f32, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.25),
                },
                ..Collider::default()
            },
        ));
    }
    app
}

/// P-307 acceptance: the Physics module samples the world's step time once per repaint and charts
/// it over the history; bodies, contacts, islands and awake bodies come with it, and the play
/// world's samples (`note_play_world`) chart the same way. The metric is also a telemetry counter.
#[test]
fn the_physics_module_charts_step_time() {
    let mut app = physics_app();
    for frame_nr in 0..20u64 {
        app.update();
        collect(app.world_mut(), frame_nr);
        // A second window pass in the same egui frame does not sample again.
        collect(app.world_mut(), frame_nr);
    }
    let world = app.world_mut();
    let view = world.resource::<ProfilerModulesView>();
    assert_eq!(view.samples_taken, 20);
    assert_eq!(view.history_source, "edit");
    let physics = &view.history[Module::Physics.index()];
    assert_eq!(physics.len(), 20);
    let step: Vec<Option<f64>> = physics.iter().map(|s| s[0]).collect();
    assert!(
        step.iter().all(Option::is_some),
        "every sample read the step time: {step:?}"
    );
    assert!(
        step.iter().flatten().any(|ms| *ms > 0.0),
        "the world stepped, so some step took time: {step:?}"
    );
    let latest = physics.back().expect("sampled");
    assert_eq!(latest[1], Some(4.0), "the ground and three boxes");
    assert!(latest[3].is_some_and(|islands| islands >= 1.0), "{latest:?}");
    assert_eq!(
        chart_points(&step, 400.0, CHART_HEIGHT).len(),
        20,
        "one point per sample"
    );
    // The agent twin charts step_ms by default and answers its summary.
    let out = handler("editor.profiler_modules")(world, &json!({ "module": "physics" })).expect("physics");
    assert_eq!(out["chart"]["metric"], "step_ms");
    assert_eq!(out["metrics"][0]["key"], "step_ms");
    assert!(
        out["metrics"][0]["summary"]["max"]
            .as_f64()
            .is_some_and(|m| m > 0.0),
        "{out:#}"
    );
    assert!(out["chart"]["points"].as_u64().is_some_and(|n| n >= 20));
    assert!(
        counters::listing().names.iter().any(|n| n == "physics.step_ms"),
        "module metrics are listed as counters for the Monitors tab"
    );
    let err =
        handler("editor.profiler_modules")(world, &json!({ "module": "sound" })).expect_err("bad module");
    assert_eq!(code(&err).as_deref(), Some("profiler_bad_module"));
    let err = handler("editor.profiler_modules")(world, &json!({ "module": "physics", "metric": "fps" }))
        .expect_err("bad metric");
    assert_eq!(code(&err).as_deref(), Some("profiler_bad_metric"));
    // The play world's samples replace the edit history (another source).
    let mut editor = World::new();
    note_play_world(&mut editor, app.world());
    let view = editor.resource::<ProfilerModulesView>();
    assert_eq!(view.history_source, "pie");
    assert_eq!(view.history[Module::Physics.index()].len(), 1);
}

/// The module tab's chart picker is reached with Tab and picks a metric with Space; the tab has no
/// unnamed widget.
#[test]
fn a_module_metric_is_picked_from_the_keyboard() {
    let mut world = World::new();
    world.insert_resource(regia::physics::PhysicsWorld::default());
    collect(&mut world, 1);
    let ctx = egui::Context::default();
    let size = [900.0, 500.0];
    let mut draw = |ui: &mut egui::Ui| module_tab(ui, &mut world, Module::Physics);
    frame(&ctx, size, vec![], &mut draw);
    let update = frame(&ctx, size, vec![], &mut draw);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    tab_to(&ctx, size, "Bodies", false, 10, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    assert_eq!(
        world.resource::<ProfilerModulesView>().chart_metric[Module::Physics.index()],
        1,
        "Bodies is charted"
    );
}

/// P-312 acceptance: a counter a game reports through the telemetry counter API appears in the
/// Monitors tab's list; Space puts it on the chart, and its row shows last, min, max and average.
/// The agent twin lists and charts it the same way.
#[test]
fn a_game_counter_appears_in_monitors() {
    let name = "monitors_test_game.enemies_alive";
    for v in [2.0, 9.0, 4.0] {
        regia::telemetry::counters::counter(name, v);
    }
    let mut world = World::new();
    world.insert_resource(ProfilerModulesView {
        monitor_filter: "monitors_test_game".into(),
        ..ProfilerModulesView::default()
    });
    let ctx = egui::Context::default();
    let size = [900.0, 600.0];
    let mut draw = |ui: &mut egui::Ui| monitors_tab(ui, &mut world);
    frame(&ctx, size, vec![], &mut draw);
    let update = frame(&ctx, size, vec![], &mut draw);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    let row = format!("{name}, not shown");
    tab_to(&ctx, size, &row, false, 10, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    let after = frame(&ctx, size, vec![], &mut draw);
    assert_eq!(
        focused_name(&after).as_deref(),
        Some(format!("{name}, shown").as_str())
    );
    assert_eq!(world.resource::<ProfilerModulesView>().monitors, [name]);
    let rows = monitor_rows(&world.resource::<ProfilerModulesView>().monitors);
    assert_eq!(
        monitor_text(&rows[0]),
        format!("{name}: last 4, min 2, max 9, avg 5")
    );
    // The agent twin: listed, charted, summarized; an unknown counter is refused.
    let monitors = handler("editor.profiler_monitors");
    let mut agent_world = World::new();
    let out = monitors(
        &mut agent_world,
        &json!({ "add": name, "filter": "monitors_test_game" }),
    )
    .expect("add");
    assert_eq!(out["counters"], json!([name]));
    assert_eq!(out["shown"][0]["name"], name);
    assert_eq!(out["shown"][0]["summary"]["max"], 9.0);
    assert_eq!(out["shown"][0]["summary"]["avg"], 5.0);
    let err = monitors(&mut agent_world, &json!({ "add": "monitors_test_game.never" })).expect_err("unknown");
    assert_eq!(code(&err).as_deref(), Some("profiler_unknown_counter"));
    let out = monitors(&mut agent_world, &json!({ "remove": name })).expect("remove");
    assert_eq!(out["shown"], json!([]));
    // Built-in engine counters are listed too.
    let out = monitors(&mut agent_world, &json!({ "filter": "core." })).expect("builtins");
    assert!(
        out["counters"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == "core.asset_loads")),
        "{out:#}"
    );
}

/// Several counters share one chart; the ninth pushes the oldest off.
#[test]
fn the_chart_keeps_at_most_eight_counters() {
    let mut view = ProfilerModulesView::default();
    for i in 0..=MAX_MONITORS {
        toggle_monitor(&mut view, &format!("c{i}"));
    }
    assert_eq!(view.monitors.len(), MAX_MONITORS);
    assert_eq!(view.monitors[0], "c1");
    toggle_monitor(&mut view, "c3");
    assert!(
        !view.monitors.iter().any(|n| n == "c3"),
        "a second toggle removes it"
    );
}

fn load_row(asset: &str, bytes: u64, total_ms: f64, refs: usize) -> LoadRow {
    LoadRow {
        id: format!("id-{asset}"),
        path: Some(asset.to_owned()),
        kind: "mesh",
        bytes,
        read_ms: total_ms / 2.0,
        decode_ms: total_ms / 2.0,
        upload_ms: 0.0,
        total_ms,
        refs,
        source: "cache",
        ok: true,
    }
}

/// The Loads table sorts by any column, both ways; ties keep the load order.
#[test]
fn loads_sort_by_every_column() {
    let rows = vec![
        load_row("b.mesh", 300, 1.0, 2),
        load_row("a.mesh", 100, 3.0, 0),
        load_row("c.mesh", 200, 2.0, 1),
    ];
    let order = |column, descending| {
        let mut r = rows.clone();
        sort_loads(&mut r, column, descending);
        r.iter().map(|r| r.asset().to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(order(LoadColumn::Total, true), ["a.mesh", "c.mesh", "b.mesh"]);
    assert_eq!(order(LoadColumn::Size, false), ["a.mesh", "c.mesh", "b.mesh"]);
    assert_eq!(order(LoadColumn::Asset, false), ["a.mesh", "b.mesh", "c.mesh"]);
    assert_eq!(order(LoadColumn::Refs, true), ["b.mesh", "c.mesh", "a.mesh"]);
    assert_eq!(
        order(LoadColumn::Kind, false),
        ["b.mesh", "a.mesh", "c.mesh"],
        "ties keep load order"
    );
    for c in LoadColumn::ALL {
        assert_eq!(LoadColumn::from_key(c.key()), Some(c));
    }
    assert!(load_text(&rows[0]).starts_with("b.mesh, mesh, 300 bytes"));
}

/// The Loads tab's sort headers are buttons reached with Tab and pressed with Space; without an
/// asset server the tab says so and the agent twin answers no rows; a bad column is refused.
#[test]
fn loads_sort_from_the_keyboard_and_the_method_refuses_a_bad_column() {
    let dir = regia_test_utils::unique_temp_dir("profiler-loads");
    let cache = regia_asset::cooked_cache_dir(&dir, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    regia_asset::AssetManifest::default()
        .write_to_cache(&cache)
        .expect("manifest");
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: dir.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    let ctx = egui::Context::default();
    let size = [900.0, 500.0];
    let mut draw = |ui: &mut egui::Ui| loads_tab(ui, &mut world);
    frame(&ctx, size, vec![], &mut draw);
    let update = frame(&ctx, size, vec![], &mut draw);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    tab_to(&ctx, size, "Size", false, 10, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    assert_eq!(
        world.resource::<ProfilerModulesView>().load_sort,
        (LoadColumn::Size, true),
        "Space on the Size header sorts by size, largest first"
    );
    let loads = handler("editor.profiler_loads");
    let out = loads(&mut world, &json!({ "sort": "read", "descending": false })).expect("loads");
    assert_eq!(
        (out["sort"].clone(), out["descending"].clone()),
        (json!("read"), json!(false))
    );
    assert_eq!(out["rows"], json!([]));
    let err = loads(&mut world, &json!({ "sort": "speed" })).expect_err("bad sort");
    assert_eq!(code(&err).as_deref(), Some("profiler_bad_sort"));
    let err = loads(&mut world, &json!({ "limit": 0 })).expect_err("bad limit");
    assert_eq!(code(&err).as_deref(), Some("profiler_bad_limit"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent twin of the History graph's marks: a level load the edit world reports and a trace
/// mark recorded in the same frame are answered at that frame's history index once the frame has
/// ended, and not while it is still in progress (outside the history); every mark points into the
/// answered history.
#[test]
fn the_marks_method_answers_level_loads_and_trace_marks_at_their_frame() {
    let profiler = regia_core::profiling::FrameProfiler::new();
    for _ in 0..4 {
        profiler.end_frame();
    }
    let mut world = World::new();
    world.insert_resource(regia::Profiler(profiler.clone()));
    world.insert_resource(crate::profiler_source::ProfilerView::default());
    world.insert_resource(Messages::<regia_scene::level::LevelLoaded>::default());
    // A distinct entity count names this test's level load among other tests' marks.
    let entities = 100_000 + std::process::id() as usize % 100_000;
    world.write_message(regia_scene::level::LevelLoaded {
        level: regia_scene::level::LevelId::default(),
        entities,
    });
    let level_label = format!(
        "level {} loaded ({entities} entities)",
        regia_scene::level::LevelId::default()
    );
    let marks = handler("editor.profiler_marks");
    // The call samples the world: the level load is noted in the frame in progress, which is not
    // in the history yet.
    let out = marks(&mut world, &json!({})).expect("marks");
    assert_eq!(out["history"]["frames"], 4, "{out:#}");
    let listed = |out: &Value, prefix: &str| -> Vec<Value> {
        out["marks"]
            .as_array()
            .expect("marks array")
            .iter()
            .filter(|m| m["label"].as_str().is_some_and(|l| l.starts_with(prefix)))
            .cloned()
            .collect()
    };
    assert_eq!(listed(&out, &level_label), Vec::<Value>::new(), "{out:#}");
    let trace_name = format!("profiler_marks_method_test_{}", std::process::id());
    // Tag the mark with this test's profiler (the board's profiler is process-wide).
    counters::attach_frame_profiler(&profiler);
    let _ = regia::telemetry::trace::recorder().mark(&trace_name, None);
    let marked = profiler.newest_frame_number().expect("four frames ended") + 1;
    for _ in 0..2 {
        profiler.end_frame();
    }
    let out = marks(&mut world, &json!({})).expect("marks after the frame ended");
    let frames = out["history"]["frames"].as_u64().expect("frames") as usize;
    assert_eq!(frames, profiler.frame_numbers().len(), "{out:#}");
    let index = profiler
        .frame_numbers()
        .iter()
        .position(|n| *n == marked)
        .expect("the marked frame is in the history");
    let level = listed(&out, &level_label);
    assert_eq!(level.len(), 1, "the level load is listed once: {out:#}");
    assert_eq!(
        (&level[0]["index"], &level[0]["kind"], &level[0]["frame"]),
        (&json!(index), &json!("level_load"), &json!(marked)),
        "{out:#}"
    );
    let trace = listed(&out, &trace_name);
    assert_eq!(trace.len(), 1, "the trace mark is listed once: {out:#}");
    assert_eq!(
        (&trace[0]["index"], &trace[0]["kind"], &trace[0]["frame"]),
        (&json!(index), &json!("trace"), &json!(marked)),
        "{out:#}"
    );
    for m in out["marks"].as_array().expect("marks array") {
        assert!(
            m["index"].as_u64().is_some_and(|i| (i as usize) < frames),
            "every mark points into the history: {m}"
        );
    }
    // The level load is noted once: a later call does not list it again.
    let again = marks(&mut world, &json!({})).expect("marks again");
    assert_eq!(listed(&again, &level_label).len(), 1, "{again:#}");
}

/// Marks land on the history index of their frame; marks of frames outside the history are left
/// out; hitches are marks too.
#[test]
fn marks_map_to_history_indices_by_frame_number() {
    let sample = |ms: f64| crate::profiler_model::FrameSample {
        ms,
        spans: Vec::new(),
    };
    let history = vec![sample(5.0), sample(5.0), sample(30.0), sample(5.0)];
    let numbers = [10, 11, 12, 13];
    let marks = vec![
        MarkEntry {
            seq: 0,
            name: "boss".into(),
            kind: "trace".into(),
            frame: 11,
        },
        MarkEntry {
            seq: 1,
            name: "level 7 loaded (3 entities)".into(),
            kind: "level_load".into(),
            frame: 13,
        },
        MarkEntry {
            seq: 2,
            name: "gone".into(),
            kind: "trace".into(),
            frame: 2,
        },
    ];
    let out = history_marks(&marks, &numbers, &history, &[2]);
    let got: Vec<(usize, &str, &str)> = out
        .iter()
        .map(|m| (m.index, m.kind.as_str(), m.label.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1, "trace", "boss (trace, frame 11)"),
            (2, "hitch", "hitch: 30.00 ms"),
            (
                3,
                "level_load",
                "level 7 loaded (3 entities) (level load, frame 13)"
            ),
        ]
    );
}
