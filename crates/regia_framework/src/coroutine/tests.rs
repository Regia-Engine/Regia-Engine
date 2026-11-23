// SPDX-License-Identifier: MPL-2.0
//! Executor tests: start order, owner cancellation, waits and world access.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::*;

/// Event log the coroutines append to (through `Co::queue`).
#[derive(Resource, Default)]
struct Log(Vec<String>);

#[derive(Resource, Default)]
struct Flag(bool);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CoroutinePlugin))
        .init_resource::<Log>()
        .init_resource::<Flag>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app
}

fn log(co: &Co, entry: impl Into<String>) {
    let entry = entry.into();
    co.queue(move |world| world.resource_mut::<Log>().0.push(entry));
}

fn entries(app: &App) -> Vec<String> {
    app.world().resource::<Log>().0.clone()
}

/// Logs `name0`, `name1`, ... one per frame.
fn ticker(
    name: &'static str,
    frames: u32,
) -> impl FnOnce(Co) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    move |co| {
        Box::pin(async move {
            for i in 0..frames {
                log(&co, format!("{name}{i}"));
                co.next_frame().await;
            }
        })
    }
}

#[test]
fn coroutines_run_in_start_order_one_step_per_frame() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, ticker("a", 3));
    start_coroutine(app.world_mut(), None, ticker("b", 3));
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(entries(&app), ["a0", "b0", "a1", "b1", "a2", "b2"]);
    app.update();
    assert!(app.world().resource::<Coroutines>().is_empty(), "both finished");
}

#[test]
fn reused_slots_keep_start_order() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move { log(&co, "first") });
    start_coroutine(app.world_mut(), None, ticker("b", 2));
    app.update(); // `first` finishes and frees slot 0
    let late = start_coroutine(app.world_mut(), None, ticker("c", 2));
    assert_eq!(late.id().index(), 0, "the late coroutine reuses the freed slot");
    app.update();
    assert_eq!(
        entries(&app),
        ["first", "b0", "b1", "c0"],
        "c runs after b: start order, not slot order"
    );
}

#[test]
fn a_coroutine_started_by_a_coroutine_first_runs_next_frame() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        log(&co, "parent");
        co.queue(|world| {
            start_coroutine(world, None, |co| async move { log(&co, "child") });
        });
        co.next_frame().await;
        log(&co, "parent again");
    });
    app.update();
    assert_eq!(entries(&app), ["parent"]);
    app.update();
    assert_eq!(entries(&app), ["parent", "parent again", "child"]);
}

/// Counts drops, to prove cancellation drops the coroutine's locals.
struct DropGuard(Arc<AtomicU32>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn despawning_the_owner_cancels_the_coroutine() {
    let mut app = app();
    let owner = app.world_mut().spawn(()).id();
    let dropped = Arc::new(AtomicU32::new(0));
    let guard = DropGuard(dropped.clone());
    let unowned = start_coroutine(app.world_mut(), None, ticker("free", 10));
    let owned = start_coroutine(app.world_mut(), Some(owner), move |co| async move {
        let _guard = guard;
        for i in 0..10 {
            log(&co, format!("owned{i}"));
            co.next_frame().await;
        }
    });
    app.update();
    app.update();
    app.world_mut().despawn(owner);
    app.update();
    app.update();
    let owned_entries: Vec<String> = entries(&app)
        .into_iter()
        .filter(|e| e.starts_with("owned"))
        .collect();
    assert_eq!(
        owned_entries,
        ["owned0", "owned1"],
        "nothing runs after the despawn"
    );
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the future and its locals were dropped"
    );
    let mut hub = app.world_mut().resource_mut::<Coroutines>();
    assert!(hub.is_done(owned));
    assert!(!hub.is_done(unowned), "unowned coroutines keep running");
    assert_eq!(hub.len(), 1);
}

#[test]
fn stop_and_stop_owned_cancel() {
    let mut app = app();
    let owner = app.world_mut().spawn(()).id();
    let a = start_coroutine(app.world_mut(), Some(owner), ticker("a", 10));
    let b = start_coroutine(app.world_mut(), Some(owner), ticker("b", 10));
    let c = start_coroutine(app.world_mut(), None, ticker("c", 10));
    app.update();
    {
        let mut hub = app.world_mut().resource_mut::<Coroutines>();
        hub.stop(c);
        hub.stop_owned(owner);
        assert!(hub.is_done(a) && hub.is_done(b) && hub.is_done(c));
        assert!(hub.is_empty());
    }
    app.update();
    assert_eq!(entries(&app), ["a0", "b0", "c0"]);
}

#[test]
fn a_coroutine_can_stop_itself_from_a_queued_closure() {
    let mut app = app();
    let handle = Arc::new(Mutex::new(None::<Coroutine>));
    let own = handle.clone();
    let started = start_coroutine(app.world_mut(), None, move |co| async move {
        for i in 0..5 {
            log(&co, format!("self{i}"));
            if i == 1 {
                let own = own.clone();
                co.queue(move |world| {
                    if let Some(me) = *own.lock().unwrap_or_else(PoisonError::into_inner) {
                        world.resource_mut::<Coroutines>().stop(me);
                    }
                });
            }
            co.next_frame().await;
        }
    });
    *handle.lock().unwrap_or_else(PoisonError::into_inner) = Some(started);
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(entries(&app), ["self0", "self1"]);
}

#[test]
fn wait_until_resumes_on_the_frame_the_condition_holds() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        log(&co, "waiting");
        co.wait_until(|world| world.resource::<Flag>().0).await;
        let frame = co.frame();
        log(&co, format!("done at {frame}"));
    });
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(entries(&app), ["waiting"]);
    app.world_mut().resource_mut::<Flag>().0 = true;
    app.update();
    let frame = app.world().resource::<Time>().frame_count();
    assert_eq!(entries(&app), ["waiting".to_string(), format!("done at {frame}")]);
}

/// Calls back into the coroutine from `drop`, like user state owning a `Co` handle would.
struct QueueOnDrop(Co);

impl Drop for QueueOnDrop {
    fn drop(&mut self) {
        log(&self.0, "dropped");
    }
}

/// A predicate whose captured state queues work when dropped must not deadlock the executor:
/// the condition is dropped after the `Co` lock is released, both when it is met and when the
/// coroutine is cancelled mid-wait.
#[test]
fn wait_until_drops_captured_state_outside_the_lock() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        let guard = QueueOnDrop(co.clone());
        co.wait_until(move |world| {
            let _keep = &guard;
            world.resource::<Flag>().0
        })
        .await;
        log(&co, "resumed");
    });
    app.update();
    app.world_mut().resource_mut::<Flag>().0 = true;
    app.update();
    app.update();
    let seen = entries(&app);
    assert!(seen.contains(&"dropped".to_string()), "{seen:?}");
    assert!(seen.contains(&"resumed".to_string()), "{seen:?}");
}

#[test]
fn wait_until_that_already_holds_does_not_cost_a_frame() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        // The queued closure runs before the predicate is first evaluated.
        co.queue(|world| world.resource_mut::<Flag>().0 = true);
        co.wait_until(|world| world.resource::<Flag>().0).await;
        log(&co, "same frame");
    });
    app.update();
    assert_eq!(entries(&app), ["same frame"]);
}

#[test]
fn wait_seconds_resumes_on_the_same_fixed_step_under_different_frame_pacing() {
    fn run(frame_steps: &[u32]) -> Vec<usize> {
        #[derive(Resource, Default)]
        struct Seen(Vec<usize>);

        let mut app = app();
        app.world_mut().resource_mut::<FixedTime>().max_substeps = 8;
        start_coroutine(app.world_mut(), None, |co| async move {
            co.wait_seconds(0.05).await;
            log(&co, "done");
        });
        app.init_resource::<Seen>().add_systems(
            FixedUpdate,
            (|log: Res<Log>, mut seen: ResMut<Seen>| {
                seen.0.push(log.0.len());
            })
            .after(CoroutineSet),
        );
        // The initial poll happens in Update; counting starts with the following fixed steps.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
        app.update();
        let step = app.world().resource::<FixedTime>().step();
        for &steps in frame_steps {
            app.insert_resource(TimeUpdateStrategy::ManualDuration(step * steps));
            app.update();
        }
        app.world_mut().remove_resource::<Seen>().unwrap_or_default().0
    }

    let hitch = run(&[5]);
    let short = run(&[1, 1, 1, 1, 1]);
    assert_eq!(
        hitch,
        [0, 0, 1, 1, 1],
        "three counted fixed steps resume the wait"
    );
    assert_eq!(short, hitch, "frame partitioning does not change the resume step");
}

#[test]
fn frame_waits_do_not_advance_during_a_fixed_step_hitch() {
    #[derive(Resource, Default)]
    struct Seen(Vec<usize>);

    let mut app = app();
    app.world_mut().resource_mut::<FixedTime>().max_substeps = 8;
    start_coroutine(app.world_mut(), None, |co| async move {
        co.next_frame().await;
        log(&co, "done");
    });
    app.init_resource::<Seen>().add_systems(
        FixedUpdate,
        (|log: Res<Log>, mut seen: ResMut<Seen>| {
            seen.0.push(log.0.len());
        })
        .after(CoroutineSet),
    );
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
    app.update();
    let step = app.world().resource::<FixedTime>().step();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step * 3));
    app.update();

    assert_eq!(
        app.world_mut().remove_resource::<Seen>().unwrap_or_default().0,
        [0, 0, 0],
        "none of the hitch's fixed steps completes an application-frame wait"
    );
    assert_eq!(entries(&app), ["done"], "the following Update completes it once");
}

#[test]
fn wait_seconds_follows_game_time_and_pauses() {
    let mut app = app();
    app.world_mut().resource_mut::<FixedTime>().hz = 10.0;
    start_coroutine(app.world_mut(), None, |co| async move {
        co.wait_seconds(0.5).await;
        log(&co, "half a second");
    });
    app.update(); // the wait starts
    for _ in 0..4 {
        app.update();
    }
    assert!(entries(&app).is_empty(), "0.4 s counted");
    app.world_mut().resource_mut::<Time>().paused = true;
    for _ in 0..5 {
        app.update();
    }
    assert!(entries(&app).is_empty(), "paused frames add no game time");
    app.world_mut().resource_mut::<Time>().paused = false;
    app.update();
    assert_eq!(entries(&app), ["half a second"]);
}

/// ADR-10105: a frame wait holds through a gameplay pause (paused frames do not count) and
/// finishes the unpaused frames it still owed after the pause.
#[test]
fn wait_frames_hold_while_the_game_is_paused() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        co.wait_frames(3).await;
        log(&co, "three frames");
    });
    app.update(); // the wait starts
    app.update(); // one frame counted
    app.world_mut().resource_mut::<Time>().paused = true;
    for _ in 0..300 {
        app.update();
    }
    assert!(entries(&app).is_empty(), "300 paused frames count as none");
    app.world_mut().resource_mut::<Time>().paused = false;
    app.update();
    assert!(entries(&app).is_empty(), "two counted, one still owed");
    app.update();
    assert_eq!(entries(&app), ["three frames"]);
}

#[test]
fn wait_seconds_does_not_finish_early_from_absolute_epsilon() {
    #[derive(Resource, Default)]
    struct Steps(u32);

    let mut app = app();
    app.world_mut().resource_mut::<FixedTime>().hz = 1_000_000_000.0;
    start_coroutine(app.world_mut(), None, |co| async move {
        co.wait_seconds(0.000005).await;
        log(&co, "done");
    });
    app.init_resource::<Steps>()
        .add_systems(FixedUpdate, |mut steps: ResMut<Steps>| steps.0 += 1);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
    app.update();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(1)));
    app.update();

    assert_eq!(app.world().resource::<Steps>().0, 1, "one 1 ns fixed step ran");
    assert!(entries(&app).is_empty(), "1 ns does not satisfy a 5 us wait");
}

#[test]
fn presentation_delta_does_not_enlarge_fixed_wait_epsilon() {
    let co = Co::default();
    let mut wait = Box::pin(co.wait_seconds(0.100_02));
    let mut context = Context::from_waker(Waker::noop());

    co.begin_pass(0.1, 1, 1, 1);
    assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
    co.begin_pass(0.1, 1, 1, 2);
    assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
    co.begin_pass(100.0, 2, 2, 2);
    assert_eq!(
        wait.as_mut().poll(&mut context),
        Poll::Pending,
        "an Update poll at the same fixed tick cannot finish the wait"
    );
}

/// corr-low-framework (corr-framework-a L1): a long wait at a fixed rate whose `f32` step rounds
/// down (50 Hz: `0.02` is stored as `0.019999999552…`) resumes on its own step. The per-step slack
/// `delta * 1e-4` (2 µs) is under the 2.2 µs the 5000 rounded steps of a 100 s wait fall short,
/// so the wait resumed one fixed step late.
#[test]
fn a_long_wait_resumes_on_its_step_when_the_f32_step_rounds_down() {
    let co = Co::default();
    let mut wait = Box::pin(co.wait_seconds(100.0));
    let mut context = Context::from_waker(Waker::noop());
    let delta = 1.0f32 / 50.0;
    assert!(f64::from(delta) < 0.02, "the premise: the f32 step rounds down");
    co.begin_pass(delta, 0, 0, 0);
    assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending, "the wait starts");
    let mut resumed = None;
    for tick in 1..=5001u64 {
        co.begin_pass(delta, tick, tick, tick);
        if wait.as_mut().poll(&mut context).is_ready() {
            resumed = Some(tick);
            break;
        }
    }
    assert_eq!(resumed, Some(5000), "100 s at 50 Hz is 5000 fixed steps");
}

/// The slack never finishes a wait a whole step early, however long it runs: at 1 kHz, after
/// 50 000 counted steps, a wait one step longer is still pending.
#[test]
fn the_relative_wait_slack_stays_under_one_step() {
    let co = Co::default();
    let mut wait = Box::pin(co.wait_seconds(50.001));
    let mut context = Context::from_waker(Waker::noop());
    let delta = 1.0f32 / 1000.0;
    co.begin_pass(delta, 0, 0, 0);
    assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
    let mut resumed = None;
    for tick in 1..=50_002u64 {
        co.begin_pass(delta, tick, tick, tick);
        if wait.as_mut().poll(&mut context).is_ready() {
            resumed = Some(tick);
            break;
        }
    }
    assert_eq!(resumed, Some(50_001));
}

#[test]
fn wait_frames_counts_frames_and_zero_waits_are_immediate() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        co.wait_frames(0).await;
        co.wait_seconds(0.0).await;
        log(&co, "start");
        co.wait_frames(3).await;
        log(&co, "three frames later");
    });
    app.update();
    assert_eq!(entries(&app), ["start"]);
    app.update();
    app.update();
    assert_eq!(entries(&app), ["start"]);
    app.update();
    assert_eq!(entries(&app), ["start", "three frames later"]);
}

#[test]
fn with_world_resumes_in_the_same_frame_with_the_result() {
    let mut app = app();
    let owner = app.world_mut().spawn(Name::new("target")).id();
    let job = start_coroutine(app.world_mut(), Some(owner), |co| async move {
        let owner = co.owner().unwrap_or(Entity::PLACEHOLDER);
        let name = co
            .with_world(move |world| world.get::<Name>(owner).map(|n| n.as_str().to_string()))
            .await;
        let frame = co
            .with_world(|world| world.resource::<Time>().frame_count())
            .await;
        (name, frame)
    });
    app.update();
    let mut hub = app.world_mut().resource_mut::<Coroutines>();
    assert!(hub.is_done(job));
    assert_eq!(hub.take_result(job), Some((Some("target".to_string()), 1)));
    assert_eq!(hub.take_result(job), None, "a result is taken once");
}

#[test]
fn results_and_stale_handles() {
    let mut app = app();
    let answer = start_coroutine(app.world_mut(), None, |co| async move {
        co.next_frame().await;
        42_u32
    });
    let unit = start_coroutine(app.world_mut(), None, |_co| async {});
    {
        let mut hub = app.world_mut().resource_mut::<Coroutines>();
        assert_eq!(hub.take_result(answer), None, "not finished");
        assert_eq!(hub.take_result(unit), None, "not run yet");
    }
    app.update();
    app.update();
    let mut hub = app.world_mut().resource_mut::<Coroutines>();
    assert_eq!(hub.take_result(unit), Some(()));
    assert_eq!(hub.take_result(answer), Some(42));
    let reuse = hub.start(None, |_co| async { 7_u32 });
    assert_eq!(
        hub.take_result(answer),
        None,
        "a stale handle misses the coroutine in its slot"
    );
    assert!(!hub.is_done(reuse));
}

#[test]
fn a_coroutine_that_never_yields_a_frame_is_parked() {
    let mut app = app();
    start_coroutine(app.world_mut(), None, |co| async move {
        loop {
            co.wait_until(|_| true).await;
        }
    });
    app.update(); // must return
    app.update();
    assert_eq!(app.world().resource::<Coroutines>().len(), 1);
}

#[test]
fn list_reports_labels_owners_and_waits() {
    let mut app = app();
    let owner = app.world_mut().spawn(()).id();
    app.world_mut()
        .resource_mut::<Coroutines>()
        .start_labeled("intro", Some(owner), |co| async move {
            co.wait_seconds(10.0).await;
        });
    start_coroutine(app.world_mut(), None, |co| async move {
        co.wait_until(|_| false).await;
    });
    app.update();
    let list = app.world().resource::<Coroutines>().list();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].label, "intro");
    assert_eq!(list[0].owner, Some(owner));
    assert!(matches!(list[0].waiting, WaitingOn::Seconds(s) if (s - 10.0).abs() < 1e-3));
    assert_eq!(list[1].waiting, WaitingOn::Condition);
    assert_eq!(list[1].waiting.name(), "condition");
}

#[test]
fn coroutines_without_the_plugin_wait_and_never_panic() {
    let mut world = World::new();
    let job = start_coroutine(&mut world, None, |_co| async { 1 });
    assert!(!world.resource_mut::<Coroutines>().is_done(job));
}

#[test]
fn script_ctx_coroutines_are_bound_to_the_script_entity() {
    let mut app = app();
    let entity = app.world_mut().spawn(()).id();
    let job = crate::script::ScriptCtx::new(app.world_mut(), entity).start_coroutine(|co| async move {
        assert_eq!(co.owner(), Some(entity));
        co.wait_frames(100).await;
    });
    app.update();
    app.world_mut().despawn(entity);
    app.update();
    assert!(app.world_mut().resource_mut::<Coroutines>().is_done(job));
}
