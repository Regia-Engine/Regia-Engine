// SPDX-License-Identifier: MPL-2.0
//! Runner lifecycle tests (spec 41 § 8, tests 1-11). Frame counts only; no clocks, no sleeps.

use std::collections::BTreeMap;

use regia_core::math::Vec3;
use regia_render::Camera;
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Name};
use serde_json::{Value, json};

use super::context::{RunData, Shared};
use super::expected::ExpectedStore;
use super::*;

#[derive(Resource, Default)]
struct Log(Vec<String>);

#[derive(Resource, Default)]
struct Counter(u32);

#[derive(Resource, Default)]
struct SettingsStore(BTreeMap<String, Value>);

#[derive(Resource, Default)]
struct Reloads(u32);

fn log(cx: &mut TestContext, text: &str) {
    cx.world().resource_mut::<Log>().0.push(text.to_owned());
}

fn count_up(mut counter: ResMut<Counter>) {
    counter.0 += 1;
}

fn eval(world: &mut World, condition: &Value) -> Result<(bool, String), String> {
    let n = condition
        .get("counter_gte")
        .and_then(Value::as_u64)
        .ok_or("unsupported condition")?;
    let have = u64::from(world.resource::<Counter>().0);
    Ok((have >= n, format!("counter = {have}, expected >= {n}")))
}

fn get_setting(world: &mut World, key: &str) -> Option<Value> {
    world.resource::<SettingsStore>().0.get(key).cloned()
}

fn set_setting(world: &mut World, key: &str, value: &Value) -> Result<(), String> {
    world
        .resource_mut::<SettingsStore>()
        .0
        .insert(key.to_owned(), value.clone());
    Ok(())
}

fn reload(world: &mut World) -> Result<(), String> {
    world.resource_mut::<Reloads>().0 += 1;
    Ok(())
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, FunctionalTestPlugin))
        .init_resource::<Log>()
        .init_resource::<Counter>()
        .init_resource::<Reloads>()
        .init_resource::<SettingsStore>()
        .add_systems(Update, count_up.before(FunctionalTestSet));
    app.world_mut()
        .resource_mut::<SettingsStore>()
        .0
        .insert("render.quality".into(), json!("high"));
    app.world_mut().resource_mut::<Runner>().set_hooks(TestHooks {
        condition: Some(eval),
        get_setting: Some(get_setting),
        set_setting: Some(set_setting),
        reload_level: Some(reload),
        ..TestHooks::default()
    });
    app
}

fn place(app: &mut App, name: &str, body: &str, tweak: impl FnOnce(&mut FunctionalTest)) -> Entity {
    let mut test = FunctionalTest {
        body: TestBodyRef::Rust(body.to_owned()),
        ..FunctionalTest::default()
    };
    tweak(&mut test);
    app.world_mut().spawn((Name::new(name), test)).id()
}

fn path<T>() -> String {
    std::any::type_name::<T>().to_owned()
}

fn run(app: &mut App, selection: Selection, opts: RunOptions) -> TestReport {
    app.world_mut().resource_mut::<Runner>().start(selection, opts);
    for _ in 0..2000 {
        app.update();
        if app.world().resource::<Runner>().status() == RunnerStatus::Done {
            return app.world().resource::<Runner>().report().clone();
        }
    }
    panic!(
        "runner did not finish: {:?}",
        app.world().resource::<Runner>().status()
    );
}

fn run_all(app: &mut App) -> TestReport {
    run(app, Selection::default(), RunOptions::default())
}

// -- 1. lifecycle order ---------------------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    ready_polls: u32,
}

impl TestBody for Recorder {
    fn setup(&mut self, cx: &mut TestContext) {
        log(cx, "setup");
    }
    fn ready(&mut self, cx: &mut TestContext) -> bool {
        self.ready_polls += 1;
        log(cx, "ready");
        self.ready_polls > 5
    }
    fn begin(&mut self, cx: &mut TestContext) {
        log(cx, "begin");
        cx.add_pass("b");
        cx.add_pass("a");
    }
    fn tick(&mut self, cx: &mut TestContext) {
        log(cx, "tick");
        if cx.frame() >= 2 {
            let label = cx.pass_label().unwrap_or("-").to_owned();
            cx.conclude(Verdict::Passed, &format!("pass {label}"));
        }
    }
    fn conclude(&mut self, cx: &mut TestContext, _verdict: Verdict) {
        log(cx, "conclude");
        self.ready_polls = 100;
    }
    fn teardown(&mut self, cx: &mut TestContext) {
        log(cx, "teardown");
    }
}

#[test]
fn lifecycle_order_setup_once_ready_polls_begin_per_pass_teardown_last() {
    let mut app = app();
    app.register_functional_test::<Recorder>();
    place(&mut app, "life", &path::<Recorder>(), |_| {});
    let report = run_all(&mut app);
    let events = app.world().resource::<Log>().0.join(",");
    // 5 false polls then true, then per pass: begin + 2 ticks + conclude; the first pass queues b and a,
    // which run in sorted order (their ready polls return true at once because ready_polls is 100).
    assert_eq!(
        events,
        "setup,ready,ready,ready,ready,ready,ready,begin,tick,tick,conclude,ready,begin,tick,tick,conclude,ready,begin,tick,tick,conclude,teardown"
    );
    let t = &report.tests[0];
    assert_eq!(t.verdict, Verdict::Passed);
    let labels: Vec<_> = t.passes.iter().map(|p| p.label.clone()).collect();
    assert_eq!(labels, vec![None, Some("a".to_owned()), Some("b".to_owned())]);
    assert_eq!(t.passes[1].message, "pass a");
}

// -- spec 41 § 10 budgets, as counting tests (counts, not time) ------------------------------------

/// § 10: an assertion call allocates nothing on success.
#[test]
fn a_passing_assertion_allocates_nothing() {
    let mut world = World::new();
    let mut run = RunData::new("t", 0, 0, Verdict::Failed);
    let mut shared = Shared {
        opts: RunOptions::default(),
        store: ExpectedStore::in_memory(),
        hooks: TestHooks::default(),
        observed: BTreeMap::new(),
    };
    let allocated = crate::test_alloc::count(|| {
        let mut cx = TestContext::new(&mut world, &mut run, &mut shared);
        assert!(cx.near_f32(1.0, 1.0, 1e-6, "probe"));
        assert!(cx.is_true(true, "probe"));
        assert!(cx.equal(&42, &42, "probe"));
    });
    assert_eq!(
        allocated, 0,
        "a passing assertion call allocated {allocated} times"
    );
}

// -- notes reach the report (spec 41 § 2.4) -------------------------------------------------------

#[derive(Default)]
struct Noter;

impl TestBody for Noter {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.note("hello from the body");
        cx.note("a second line");
        cx.conclude(Verdict::Passed, "ok");
    }
}

#[test]
fn note_text_is_carried_into_the_report() {
    let mut app = app();
    app.register_functional_test::<Noter>();
    place(&mut app, "noter", &path::<Noter>(), |_| {});
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Passed);
    // spec 41 § 2.4: `note(text)` text is carried into the report, in order.
    assert_eq!(
        report.tests[0].notes,
        ["hello from the body".to_owned(), "a second line".to_owned()]
    );
    // ... and survives the JSON round trip (the field is skipped when empty).
    let json = report.to_json().expect("json");
    assert!(json.contains("hello from the body"), "{json}");
    let back = TestReport::from_json(&json).expect("parse");
    assert_eq!(back.without_wall_time(), report.without_wall_time());
    // A test that logged no notes keeps the report shape of format v1.
    let quiet = TestReport::new("b", 0);
    let text = quiet.to_json().expect("json");
    assert!(!text.contains("notes"), "{text}");
}

// -- 2. ready gate --------------------------------------------------------------------------------

#[derive(Default)]
struct NeverReady;

impl TestBody for NeverReady {
    fn setup(&mut self, cx: &mut TestContext) {
        log(cx, "setup");
    }
    fn ready(&mut self, _cx: &mut TestContext) -> bool {
        false
    }
    fn begin(&mut self, cx: &mut TestContext) {
        log(cx, "begin");
    }
    fn teardown(&mut self, cx: &mut TestContext) {
        log(cx, "teardown");
    }
}

#[test]
fn ready_timeout_aborts_without_begin_and_still_tears_down() {
    let mut app = app();
    app.register_functional_test::<NeverReady>();
    place(&mut app, "slow", &path::<NeverReady>(), |t| t.ready_limit = 3);
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Aborted);
    assert_eq!(report.tests[0].message, "ready_timeout");
    assert_eq!(app.world().resource::<Log>().0, vec!["setup", "teardown"]);
    assert_eq!(report.exit_code(), 2);
}

// -- 3. frame limit -------------------------------------------------------------------------------

#[derive(Default)]
struct Idle;

impl TestBody for Idle {
    fn begin(&mut self, _cx: &mut TestContext) {}
}

#[test]
fn frame_limit_expires_with_on_limit_and_zero_never_expires() {
    let mut app = app();
    app.register_functional_test::<Idle>();
    place(&mut app, "limited", &path::<Idle>(), |t| {
        t.frame_limit = 7;
        t.on_limit = Verdict::Failed;
    });
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Failed);
    assert_eq!(report.tests[0].message, "frame limit");
    assert_eq!(report.tests[0].passes[0].frames, 7);

    let mut app2 = self::app();
    app2.register_functional_test::<Idle>();
    place(&mut app2, "unlimited", &path::<Idle>(), |t| t.frame_limit = 0);
    app2.world_mut()
        .resource_mut::<Runner>()
        .start(Selection::default(), RunOptions::default());
    for _ in 0..500 {
        app2.update();
    }
    assert!(matches!(
        app2.world().resource::<Runner>().status(),
        RunnerStatus::Running { frame: 499, .. }
    ));
}

// -- 4. verdicts and panics -----------------------------------------------------------------------

#[derive(Default)]
struct Silent;
impl TestBody for Silent {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.set_frame_limit(0, Verdict::Failed);
    }
    fn tick(&mut self, cx: &mut TestContext) {
        if cx.frame() == 2 {
            // Ends the pass without a verdict by hitting the (replaced) limit path is not possible; conclude
            // with a pending verdict instead.
            cx.conclude(Verdict::Pending, "");
        }
    }
}

#[derive(Default)]
struct Twice;
impl TestBody for Twice {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.conclude(Verdict::Passed, "first");
        cx.conclude(Verdict::Failed, "second");
    }
}

#[derive(Default)]
struct Panics;
impl TestBody for Panics {
    fn begin(&mut self, _cx: &mut TestContext) {}
    fn tick(&mut self, cx: &mut TestContext) {
        cx.override_setting("render.quality", json!("low"))
            .expect("known key");
        panic!("boom {}", 7);
    }
    fn teardown(&mut self, cx: &mut TestContext) {
        log(cx, "teardown");
    }
}

#[test]
fn verdicts_undecided_second_conclude_ignored_and_panic_aborts() {
    let mut app = app();
    app.register_functional_test::<Silent>()
        .register_functional_test::<Twice>()
        .register_functional_test::<Panics>();
    place(&mut app, "a_silent", &path::<Silent>(), |_| {});
    place(&mut app, "b_twice", &path::<Twice>(), |_| {});
    place(&mut app, "c_panics", &path::<Panics>(), |_| {});
    let report = run_all(&mut app);
    let by = |n: &str| report.tests.iter().find(|t| t.name == n).expect("present");
    assert_eq!(by("a_silent").verdict, Verdict::Undecided);
    assert_eq!(by("b_twice").verdict, Verdict::Passed);
    assert_eq!(by("b_twice").message, "first");
    assert_eq!(by("c_panics").verdict, Verdict::Aborted);
    assert_eq!(by("c_panics").message, "panic: boom 7");
    assert_eq!(report.exit_code(), 2);
    // The panic did not skip teardown: the setting override is back.
    assert_eq!(
        app.world().resource::<SettingsStore>().0["render.quality"],
        json!("high")
    );
    assert_eq!(app.world().resource::<Log>().0, vec!["teardown"]);
}

// -- 5. extra passes ------------------------------------------------------------------------------

#[derive(Default)]
struct ManyPasses {
    count: usize,
    fail_label: Option<&'static str>,
}

impl TestBody for ManyPasses {
    fn begin(&mut self, cx: &mut TestContext) {
        if cx.pass_label().is_none() {
            for i in 0..self.count {
                cx.add_pass(&format!("p{i:02}"));
            }
        }
        let verdict = if cx.pass_label() == self.fail_label {
            Verdict::Failed
        } else {
            Verdict::Passed
        };
        cx.conclude(verdict, "x");
    }
}

#[test]
fn passes_run_in_sorted_order_worst_wins_and_the_33rd_is_refused() {
    let mut app = app();
    // 3 extra passes, the second fails.
    #[derive(Default)]
    struct Three(ManyPasses);
    impl TestBody for Three {
        fn begin(&mut self, cx: &mut TestContext) {
            self.0.count = 3;
            self.0.fail_label = Some("p01");
            self.0.begin(cx);
        }
    }
    app.register_functional_test::<Three>();
    place(&mut app, "three", &path::<Three>(), |_| {});
    let report = run_all(&mut app);
    let t = &report.tests[0];
    let labels: Vec<_> = t
        .passes
        .iter()
        .map(|p| p.label.clone().unwrap_or_default())
        .collect();
    assert_eq!(labels, vec!["", "p00", "p01", "p02"]);
    assert_eq!(t.verdict, Verdict::Failed, "worst pass wins");

    // 32 passes run; a 33rd queued label is refused.
    #[derive(Default)]
    struct TooMany(ManyPasses);
    impl TestBody for TooMany {
        fn begin(&mut self, cx: &mut TestContext) {
            self.0.count = 33;
            self.0.begin(cx);
        }
    }
    let mut app = self::app();
    app.register_functional_test::<TooMany>();
    place(&mut app, "many", &path::<TooMany>(), |_| {});
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Aborted);
    assert_eq!(report.tests[0].message, "too_many_passes");
    assert_eq!(report.tests[0].passes.len(), 32);
}

#[derive(Default)]
struct Eager {
    started: u32,
}
impl TestBody for Eager {
    fn begin(&mut self, cx: &mut TestContext) {
        self.started += 1;
        if self.started == 1 {
            cx.add_pass("again");
        }
        let frame = cx.world().resource::<Counter>().0;
        log(cx, &format!("begin@{frame}"));
        cx.conclude(Verdict::Passed, "");
    }
    fn wants_pass_now(&mut self, _cx: &TestContext) -> bool {
        true
    }
}

#[test]
fn wants_pass_now_starts_the_next_pass_in_the_same_frame() {
    let mut app = app();
    app.register_functional_test::<Eager>();
    place(&mut app, "eager", &path::<Eager>(), |_| {});
    run_all(&mut app);
    let log = app.world().resource::<Log>().0.clone();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0], log[1], "both passes began in one frame");
}

// -- 7. teardown ----------------------------------------------------------------------------------

#[derive(Default)]
struct Leaky;
impl TestBody for Leaky {
    fn begin(&mut self, cx: &mut TestContext) {
        cx.spawn_scoped(Name::new("scoped"));
        cx.override_setting("render.quality", json!("low"))
            .expect("known key");
        assert!(cx.override_setting("no.such", json!(1)).is_err());
        cx.conclude(Verdict::Failed, "on purpose");
    }
}

#[derive(Default)]
struct SeesClean;
impl TestBody for SeesClean {
    fn begin(&mut self, cx: &mut TestContext) {
        let quality = cx.world().resource::<SettingsStore>().0["render.quality"].clone();
        cx.equal(&quality, &json!("high"), "quality restored");
        let scoped = cx
            .world()
            .query::<&Name>()
            .iter(cx.world())
            .filter(|n| n.as_str() == "scoped")
            .count();
        cx.equal(&scoped, &0, "no scoped leftovers");
        cx.conclude(Verdict::Passed, "clean");
    }
}

#[test]
fn a_failing_test_does_not_leak_into_the_next() {
    let mut app = app();
    app.register_functional_test::<Leaky>()
        .register_functional_test::<SeesClean>();
    place(&mut app, "a_leaky", &path::<Leaky>(), |_| {});
    place(&mut app, "b_clean", &path::<SeesClean>(), |_| {});
    let before = app.world().entities().len();
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Failed);
    assert_eq!(report.tests[1].verdict, Verdict::Passed, "{:?}", report.tests[1]);
    assert_eq!(app.world().entities().len(), before, "entity count unchanged");
}

#[derive(Default)]
struct Checks;
impl TestBody for Checks {
    fn begin(&mut self, cx: &mut TestContext) {
        let adopted = cx.world().spawn(Name::new("adopted")).id();
        cx.despawn_at_teardown(adopted);
        let scoped = cx.spawn_scoped(Name::new("scoped_probe"));
        let results = [
            // Hold: no failure recorded.
            cx.entity_alive(scoped),
            cx.entity_has::<Name>(scoped),
            cx.is_true(true, "t"),
            cx.is_false(false, "f"),
            cx.relate(2, Relation::Lt, 3, "lt"),
            cx.condition(&json!({ "counter_gte": 0 })),
            // Fail: each records one message and the body keeps running.
            cx.entity_has::<FunctionalTest>(scoped),
            cx.is_false(true, "flag"),
            cx.relate(5, Relation::Le, 4, "count"),
            cx.condition(&json!({ "counter_gte": 1_000_000 })),
            cx.condition(&json!({ "bad": 1 })),
        ];
        let failures = cx.failure_count();
        let text = format!("{results:?} failures={failures}");
        log(cx, &text);
        cx.conclude(Verdict::Passed, "said passed");
    }
}

/// Spec 41 § 2.4: truth, validity, relational and condition assertions return `bool`, record deterministic
/// failure text and never stop the body; recorded failures turn a `Passed` conclude into `Failed`; an entity
/// handed to `despawn_at_teardown` is gone after teardown (§ 2.2 teardown order, § 8 test 7).
#[test]
fn context_assertions_record_failures_without_stopping_and_adopted_entities_are_despawned() {
    let mut app = app();
    app.register_functional_test::<Checks>();
    place(&mut app, "checks", &path::<Checks>(), |_| {});
    let before = app.world().entities().len();
    let report = run_all(&mut app);
    assert_eq!(
        app.world().resource::<Log>().0,
        vec!["[true, true, true, true, true, true, false, false, false, false, false] failures=5"]
    );
    let t = &report.tests[0];
    assert_eq!(t.verdict, Verdict::Failed, "failures override a Passed conclude");
    let parts: Vec<&str> = t.message.split("; ").collect();
    assert_eq!(parts.len(), 6, "{}", t.message);
    assert_eq!(parts[0], "said passed");
    assert!(
        parts[1].starts_with("entity expected to have ") && parts[1].ends_with("FunctionalTest"),
        "{}",
        parts[1]
    );
    assert_eq!(parts[2], "flag: expected false");
    assert_eq!(parts[3], "count: expected 5 <= 4");
    assert!(
        parts[4].starts_with("condition failed: counter = ") && parts[4].ends_with(", expected >= 1000000"),
        "{}",
        parts[4]
    );
    assert_eq!(
        parts[5],
        "condition could not be evaluated: unsupported condition"
    );
    assert_eq!(
        app.world().entities().len(),
        before,
        "scoped and adopted entities despawned"
    );
    // Same input in a fresh app, same text: the message carries no address or clock.
    let mut twin = self::app();
    twin.register_functional_test::<Checks>();
    place(&mut twin, "checks", &path::<Checks>(), |_| {});
    assert_eq!(run_all(&mut twin).tests[0].message, t.message);
    let mut names = app.world_mut().query::<&Name>();
    assert!(
        names
            .iter(app.world())
            .all(|n| n.as_str() != "adopted" && n.as_str() != "scoped_probe")
    );
}

#[test]
fn settings_overrides_via_the_component_apply_and_unknown_keys_abort() {
    let mut app = app();
    app.register_functional_test::<Idle>();
    place(&mut app, "good", &path::<Idle>(), |t| {
        t.frame_limit = 1;
        t.on_limit = Verdict::Passed;
        t.settings = vec![("render.quality".into(), "low".into())];
    });
    place(&mut app, "zbad", &path::<Idle>(), |t| {
        t.settings = vec![("nope".into(), "1".into())]
    });
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Passed);
    assert!(
        report.tests[1].message.starts_with("setting_unknown"),
        "{}",
        report.tests[1].message
    );
    assert_eq!(
        app.world().resource::<SettingsStore>().0["render.quality"],
        json!("high")
    );
}

// -- 8. scripts -----------------------------------------------------------------------------------

fn script_app(scripts: &[(&str, &str)]) -> (App, std::path::PathBuf) {
    let dir = regia_test_utils::unique_temp_dir("functest_scripts");
    std::fs::create_dir_all(&dir).expect("dir");
    for (name, text) in scripts {
        std::fs::write(dir.join(name), text).expect("write script");
    }
    (app(), dir)
}

fn script_test(app: &mut App, name: &str, file: &str) {
    let mut test = FunctionalTest {
        body: TestBodyRef::Script(file.to_owned()),
        ..FunctionalTest::default()
    };
    test.frame_limit = 100;
    app.world_mut().spawn((Name::new(name), test));
}

fn with_root(dir: &std::path::Path) -> RunOptions {
    RunOptions {
        project_root: Some(dir.to_path_buf()),
        ..RunOptions::default()
    }
}

#[test]
fn scripts_pass_wait_fail_by_step_name_and_record_every_failed_expectation() {
    let ok = r#"(format_version: 1, steps: [
        (name: "one", expect: [(name: "c", condition: (counter_gte: 0))]),
        (name: "wait", wait_until: Some((condition: (counter_gte: 10), within_frames: 50))),
        (name: "three", expect: [(name: "c", condition: (counter_gte: 10))]),
    ])"#;
    let timeout = r#"(format_version: 1, steps: [
        (name: "fine", expect: []),
        (name: "never", wait_until: Some((condition: (counter_gte: 1000000), within_frames: 5))),
    ])"#;
    let two = r#"(format_version: 1, steps: [
        (name: "checks", expect: [
            (name: "first", condition: (counter_gte: 999999)),
            (name: "second", condition: (counter_gte: 888888)),
        ]),
    ])"#;
    let bad = "(format_version: 1, steps: [])";
    let (mut app, dir) = script_app(&[
        ("ok.functest.ron", ok),
        ("t.functest.ron", timeout),
        ("two.functest.ron", two),
        ("bad.functest.ron", bad),
    ]);
    script_test(&mut app, "a_ok", "ok.functest.ron");
    script_test(&mut app, "b_timeout", "t.functest.ron");
    script_test(&mut app, "c_two", "two.functest.ron");
    script_test(&mut app, "d_bad", "bad.functest.ron");
    script_test(&mut app, "e_missing", "missing.functest.ron");
    let report = run(&mut app, Selection::default(), with_root(&dir));
    let by = |n: &str| report.tests.iter().find(|t| t.name == n).expect("present");
    assert_eq!(by("a_ok").verdict, Verdict::Passed, "{:?}", by("a_ok"));
    assert_eq!(
        by("a_ok")
            .steps
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "wait", "three"]
    );
    assert_eq!(by("b_timeout").verdict, Verdict::Failed);
    assert!(
        by("b_timeout").message.contains("step `never` failed"),
        "{}",
        by("b_timeout").message
    );
    assert!(
        by("b_timeout").message.contains("within 5 frames"),
        "{}",
        by("b_timeout").message
    );
    let two_msg = &by("c_two").message;
    assert!(
        two_msg.contains("expect `first` failed") && two_msg.contains("expect `second` failed"),
        "{two_msg}"
    );
    assert!(
        by("d_bad").message.starts_with("script_invalid"),
        "{}",
        by("d_bad").message
    );
    assert!(
        by("e_missing").message.starts_with("body_not_found"),
        "{}",
        by("e_missing").message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[derive(Resource, Default)]
struct Injected(Vec<String>);

fn record_inject(world: &mut World, action: &str, value: [f32; 2], frames: u32) -> Result<(), String> {
    let counter = world.resource::<Counter>().0;
    world
        .resource_mut::<Injected>()
        .0
        .push(format!("{action} {value:?} x{frames} @{counter}"));
    Ok(())
}

/// Spec 41 § 2.3: a step's `do` actions go through the agent's input injection in order and take effect on
/// the next frame, before the step's `expect`; without an injection service the step fails by name.
#[test]
fn script_inject_actions_reach_the_input_service_in_order_and_fail_the_step_without_one() {
    let walk = r#"(format_version: 1, steps: [
        (name: "walk", do: [Inject(action: "Move", value: (0.0, 1.0), frames: 90), Inject(action: "Jump", value: (1.0, 0.0), frames: 1)],
         expect: [(name: "c", condition: (counter_gte: 0))]),
        (name: "after", expect: [(name: "c", condition: (counter_gte: 0))]),
    ])"#;
    let (mut app, dir) = script_app(&[("walk.functest.ron", walk)]);
    app.init_resource::<Injected>();
    let hooks = TestHooks {
        inject: Some(record_inject),
        condition: Some(eval),
        ..TestHooks::default()
    };
    app.world_mut().resource_mut::<Runner>().set_hooks(hooks);
    script_test(&mut app, "walker", "walk.functest.ron");
    let report = run(&mut app, Selection::default(), with_root(&dir));
    let t = &report.tests[0];
    assert_eq!(t.verdict, Verdict::Passed, "{t:?}");
    assert_eq!(
        t.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["walk", "after"]
    );
    let injected = &app.world().resource::<Injected>().0;
    assert_eq!(injected.len(), 2, "{injected:?}");
    assert!(injected[0].starts_with("Move [0.0, 1.0] x90 @"), "{injected:?}");
    assert!(injected[1].starts_with("Jump [1.0, 0.0] x1 @"), "{injected:?}");
    assert_eq!(
        injected[0].rsplit('@').next(),
        injected[1].rsplit('@').next(),
        "both actions of a step are injected in the same frame"
    );

    // The default test app installs no injection service.
    let (mut bare, bare_dir) = script_app(&[("walk.functest.ron", walk)]);
    script_test(&mut bare, "walker", "walk.functest.ron");
    let report = run(&mut bare, Selection::default(), with_root(&bare_dir));
    let t = &report.tests[0];
    assert_eq!(t.verdict, Verdict::Failed);
    assert!(t.message.contains("step `walk` failed"), "{}", t.message);
    assert!(
        t.message
            .contains("inject `Move`: no input injection service installed"),
        "{}",
        t.message
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bare_dir);
}

// -- 9. expected-value store ----------------------------------------------------------------------

#[derive(Default)]
struct Expects;
impl TestBody for Expects {
    fn begin(&mut self, cx: &mut TestContext) {
        let value = cx
            .param("value")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1.0);
        cx.expect_json("answer", json!({ "v": value, "tag": "x" }));
        cx.conclude(Verdict::Passed, "");
    }
}

#[test]
fn expected_values_missing_blessed_mismatch_and_reset() {
    let dir = regia_test_utils::unique_temp_dir("functest_expected");
    std::fs::create_dir_all(&dir).expect("dir");
    let opts = |update: bool, value: &str| RunOptions {
        project_root: Some(dir.clone()),
        level: "levels/a.level.ron".into(),
        update_expected: update,
        params: BTreeMap::from([("value".to_owned(), value.to_owned())]),
        ..RunOptions::default()
    };
    let mut app = app();
    app.register_functional_test::<Expects>();
    let entity = place(&mut app, "exp", &path::<Expects>(), |_| {});

    let missing = run(&mut app, Selection::default(), opts(false, "1.0"));
    assert_eq!(missing.tests[0].verdict, Verdict::Failed);
    assert!(missing.tests[0].message.contains("no expected value"));

    let blessed = run(&mut app, Selection::default(), opts(true, "1.0"));
    assert_eq!(blessed.tests[0].verdict, Verdict::Passed);
    assert!(blessed.tests[0].blessed);
    let file = dir.join("levels/a.expected.ron");
    assert!(file.exists());

    let again = run(&mut app, Selection::default(), opts(false, "1.0"));
    assert_eq!(again.tests[0].verdict, Verdict::Passed);
    assert!(!again.tests[0].blessed);
    let within_tolerance = run(&mut app, Selection::default(), opts(false, "1.000001"));
    assert_eq!(within_tolerance.tests[0].verdict, Verdict::Passed);

    let mismatch = run(&mut app, Selection::default(), opts(false, "2.0"));
    assert_eq!(mismatch.tests[0].verdict, Verdict::Failed);
    assert!(
        mismatch.tests[0].message.contains(".v: expected 1.0, got 2.0"),
        "{}",
        mismatch.tests[0].message
    );

    app.world_mut()
        .get_mut::<FunctionalTest>(entity)
        .expect("component")
        .reset_expected = true;
    let reset = run(&mut app, Selection::default(), opts(false, "1.0"));
    assert!(
        reset.tests[0].message.contains("no expected value"),
        "reset clears the entry once"
    );
    assert!(
        !app.world()
            .get::<FunctionalTest>(entity)
            .expect("component")
            .reset_expected,
        "flag clears itself"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 41 § 2.6 / § 5 `functest.bless`: after a run that failed on a missing entry, blessing writes the
/// observed value (one key or all), refuses an unknown test or key without touching the file, and the
/// next run passes.
#[test]
fn bless_writes_the_observed_values_and_refuses_unknown_tests_and_keys() {
    let dir = regia_test_utils::unique_temp_dir("functest_bless");
    std::fs::create_dir_all(&dir).expect("dir");
    let opts = || RunOptions {
        project_root: Some(dir.clone()),
        level: "levels/b.level.ron".into(),
        ..RunOptions::default()
    };
    let mut app = app();
    app.register_functional_test::<Expects>();
    place(&mut app, "exp", &path::<Expects>(), |_| {});
    let file = dir.join("levels/b.expected.ron");

    let missing = run(&mut app, Selection::default(), opts());
    assert_eq!(missing.tests[0].verdict, Verdict::Failed);
    assert!(!file.exists(), "a failing run writes nothing");

    let mut runner = app.world_mut().resource_mut::<Runner>();
    let unknown_test = runner.bless("nobody", None).expect_err("no such test");
    assert!(unknown_test.contains("nobody"), "{unknown_test}");
    let unknown_key = runner.bless("exp", Some("nope")).expect_err("no such key");
    assert!(unknown_key.contains("nope"), "{unknown_key}");
    assert!(!file.exists(), "refused blesses write nothing");
    assert_eq!(runner.bless("exp", Some("answer")), Ok(1));
    assert_eq!(runner.bless("exp", None), Ok(1));
    let text = std::fs::read_to_string(&file).expect("written");
    assert!(text.contains("answer"), "{text}");

    let again = run(&mut app, Selection::default(), opts());
    assert_eq!(
        again.tests[0].verdict,
        Verdict::Passed,
        "{}",
        again.tests[0].message
    );
    assert!(!again.tests[0].blessed, "a bless is not an --update-expected run");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A newer `.expected.ron` aborts the run and is not rewritten, even with `update_expected`.
#[test]
fn newer_expected_file_aborts_the_run() {
    let dir = regia_test_utils::unique_temp_dir("functest_expected_newer");
    std::fs::create_dir_all(dir.join("levels")).expect("dir");
    let file = dir.join("levels/a.expected.ron");
    let text = "(format_version: 9, tests: {})";
    std::fs::write(&file, text).expect("write");
    let report = run(
        &mut app(),
        Selection::default(),
        RunOptions {
            project_root: Some(dir.clone()),
            level: "levels/a.level.ron".into(),
            update_expected: true,
            looped: true,
            ..RunOptions::default()
        },
    );
    assert_eq!(report.exit_code(), 2, "abort");
    assert_eq!(report.tests.len(), 1);
    assert_eq!(report.tests[0].verdict, Verdict::Aborted);
    assert!(!report.tests[0].blessed);
    assert!(
        report.tests[0].message.contains("levels/a.expected.ron"),
        "{}",
        report.tests[0].message
    );
    assert_eq!(std::fs::read_to_string(&file).expect("still there"), text);
    let _ = std::fs::remove_dir_all(&dir);
}

// -- 10. runner: ordering, selection, skipped, isolated, empty ------------------------------------

#[derive(Default)]
struct Pass;
impl TestBody for Pass {
    fn begin(&mut self, cx: &mut TestContext) {
        let name = cx.world().resource::<Log>().0.len();
        log(cx, &format!("ran{name}"));
        cx.conclude(Verdict::Passed, "");
    }
}

#[test]
fn ordering_selection_skipped_isolated_and_empty_runs() {
    let mut app = app();
    app.register_functional_test::<Pass>();
    place(&mut app, "zulu", &path::<Pass>(), |t| {
        t.tags = vec!["smoke".into()]
    });
    place(&mut app, "alpha", &path::<Pass>(), |t| {
        t.tags = vec!["smoke".into(), "isolated".into()]
    });
    place(&mut app, "mike", &path::<Pass>(), |t| {
        t.tags = vec!["slow".into()]
    });
    place(&mut app, "off", &path::<Pass>(), |t| t.enabled = false);
    let report = run_all(&mut app);
    // Ordered by first tag (None first), then name: "off" (no tags), then slow, then smoke: alpha, zulu.
    let order: Vec<_> = report
        .tests
        .iter()
        .filter(|t| !t.skipped)
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(order, vec!["mike", "alpha", "zulu"]);
    assert_eq!(
        app.world().resource::<Reloads>().0,
        1,
        "only the isolated test reloads the level"
    );
    let skipped = report.tests.iter().find(|t| t.skipped).expect("skipped listed");
    assert_eq!(
        (skipped.name.as_str(), skipped.verdict),
        ("off", Verdict::Pending)
    );
    assert_eq!(report.summary().skipped, 1);
    assert_eq!(report.exit_code(), 0);

    let only = run(
        &mut app,
        Selection {
            name_globs: vec!["z*".into()],
            tags: vec!["smoke".into()],
        },
        RunOptions::default(),
    );
    assert_eq!(only.tests.len(), 1);
    assert_eq!(only.tests[0].name, "zulu");

    let none = run(
        &mut app,
        Selection {
            name_globs: vec!["nothing*".into()],
            tags: vec![],
        },
        RunOptions::default(),
    );
    assert!(none.tests.is_empty());
    assert_eq!(none.exit_code(), 3, "an empty run is not a pass");
}

// -- 11. determinism ------------------------------------------------------------------------------

#[derive(Default)]
struct UsesRng;
impl TestBody for UsesRng {
    fn begin(&mut self, cx: &mut TestContext) {
        let a = cx.rng().next_u64();
        cx.conclude(Verdict::Passed, &format!("{a}"));
    }
}

#[test]
fn a_finished_test_reports_its_wall_time_and_equality_ignores_it() {
    let mut app = app();
    app.register_functional_test::<NeverReady>();
    place(&mut app, "slow", &path::<NeverReady>(), |t| t.ready_limit = 3);
    let report = run_all(&mut app);
    assert!(report.tests[0].wall_ms.is_some(), "a run test has a wall time");
    assert_eq!(report.without_wall_time().tests[0].wall_ms, None);
}

#[test]
fn two_runs_with_the_same_seed_give_equal_reports_and_other_seeds_differ() {
    let make = |seed| {
        let mut app = app();
        app.register_functional_test::<UsesRng>()
            .register_functional_test::<Pass>();
        place(&mut app, "r", &path::<UsesRng>(), |_| {});
        place(&mut app, "p", &path::<Pass>(), |_| {});
        run(
            &mut app,
            Selection::default(),
            RunOptions {
                seed,
                ..RunOptions::default()
            },
        )
    };
    assert_eq!(make(5).without_wall_time(), make(5).without_wall_time());
    assert_ne!(make(5).tests[1].message, make(6).tests[1].message);
}

#[test]
fn looped_runs_repeat_until_aborted() {
    let mut app = app();
    app.register_functional_test::<Pass>();
    place(&mut app, "p", &path::<Pass>(), |_| {});
    app.world_mut().resource_mut::<Runner>().start(
        Selection::default(),
        RunOptions {
            looped: true,
            ..RunOptions::default()
        },
    );
    for _ in 0..40 {
        app.update();
    }
    assert!(app.world().resource::<Runner>().iterations() >= 3);
    app.world_mut().resource_mut::<Runner>().abort("stop");
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(app.world().resource::<Runner>().status(), RunnerStatus::Done);
}

// -- viewpoint (spec 41 § 2.5) ---------------------------------------------------------------------

#[derive(Default)]
struct SeesCamera;

impl TestBody for SeesCamera {
    fn begin(&mut self, cx: &mut TestContext) {
        let camera = super::viewpoint::local_camera(cx.world()).expect("camera");
        let at = cx
            .world()
            .get::<Transform>(camera)
            .expect("transform")
            .translation;
        cx.world().resource_mut::<Log>().0.push(format!("camera {at}"));
        cx.conclude(Verdict::Passed, "seen");
    }
}

fn viewpoint_app() -> (App, Entity, String) {
    let mut app = app();
    app.register_functional_test::<SeesCamera>();
    let camera = app
        .world_mut()
        .spawn((
            Camera::default(),
            Transform {
                translation: Vec3::new(1.0, 2.0, 3.0),
                ..Transform::default()
            },
        ))
        .id();
    let guid = regia_core::id::Guid::from_u128(0x77);
    app.world_mut().spawn((
        EntityGuid(guid),
        Transform {
            translation: Vec3::new(5.0, 1.0, 0.0),
            ..Transform::default()
        },
    ));
    (app, camera, guid.to_string())
}

#[test]
fn the_camera_takes_the_viewpoint_at_begin_and_gets_it_back_at_teardown() {
    let (mut app, camera, guid) = viewpoint_app();
    place(&mut app, "view", &path::<SeesCamera>(), |t| {
        t.viewpoint = Some(guid)
    });
    let report = run_all(&mut app);
    assert_eq!(
        report.tests[0].verdict,
        Verdict::Passed,
        "{}",
        report.tests[0].message
    );
    assert_eq!(
        app.world().resource::<Log>().0,
        [format!("camera {}", Vec3::new(5.0, 1.0, 0.0))]
    );
    let back = app
        .world()
        .get::<Transform>(camera)
        .expect("transform")
        .translation;
    assert_eq!(back, Vec3::new(1.0, 2.0, 3.0));
    assert!(
        !app.world()
            .contains_resource::<super::viewpoint::ViewpointOverride>()
    );
}

#[test]
fn a_viewpoint_nothing_resolves_aborts_and_moves_nothing() {
    let (mut app, camera, _) = viewpoint_app();
    place(&mut app, "view", &path::<SeesCamera>(), |t| {
        t.viewpoint = Some("00000000-0000-0000-0000-000000000099".to_owned());
    });
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Aborted);
    assert!(
        report.tests[0].message.contains("bad_params"),
        "{}",
        report.tests[0].message
    );
    let at = app
        .world()
        .get::<Transform>(camera)
        .expect("transform")
        .translation;
    assert_eq!(at, Vec3::new(1.0, 2.0, 3.0));
}

#[test]
fn a_world_without_a_camera_aborts_the_test_with_the_reason() {
    let mut app = app();
    app.register_functional_test::<SeesCamera>();
    let guid = regia_core::id::Guid::from_u128(0x78);
    app.world_mut().spawn((EntityGuid(guid), Transform::default()));
    place(&mut app, "view", &path::<SeesCamera>(), |t| {
        t.viewpoint = Some(guid.to_string())
    });
    let report = run_all(&mut app);
    assert_eq!(report.tests[0].verdict, Verdict::Aborted);
    assert!(
        report.tests[0].message.contains("no camera"),
        "{}",
        report.tests[0].message
    );
}

#[test]
fn a_parented_camera_reaches_the_world_pose() {
    let (mut app, camera, guid) = viewpoint_app();
    let rig = app
        .world_mut()
        .spawn(Transform {
            translation: Vec3::new(10.0, 0.0, 0.0),
            ..Transform::default()
        })
        .id();
    app.world_mut().entity_mut(camera).insert(ChildOf(rig));
    let target: regia_core::id::Guid = guid.parse().expect("guid");
    let entity = {
        let mut q = app.world_mut().query::<(Entity, &EntityGuid)>();
        q.iter(app.world())
            .find(|(_, g)| g.0 == target)
            .expect("target")
            .0
    };
    super::viewpoint::set_viewpoint(app.world_mut(), Some(entity)).expect("set");
    let local = app
        .world()
        .get::<Transform>(camera)
        .expect("transform")
        .translation;
    assert_eq!(local, Vec3::new(-5.0, 1.0, 0.0));
    super::viewpoint::set_viewpoint(app.world_mut(), None).expect("restore");
    let back = app
        .world()
        .get::<Transform>(camera)
        .expect("transform")
        .translation;
    assert_eq!(back, Vec3::new(1.0, 2.0, 3.0));
}
