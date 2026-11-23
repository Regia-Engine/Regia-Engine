// SPDX-License-Identifier: MPL-2.0
//! A busy pool does not stall the fixed step (ADR-2565). Its own test binary: it fills the process's
//! async compute pool, which would disturb every other test in the same process.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use regia_ai::Blackboard;
use regia_ai::goap::{Action, FactTable, Goal, Planner};
use regia_ai::goap_ecs::{GoapAgent, GoapPlugin, GoapState, GoapStats, GoapStatus};
use regia_app::prelude::*;
use regia_core::tasks::{self, AsyncComputePool};

/// Every pool thread sits in a blocker while the agent's search is due. Waiting on the task would wait
/// for the blockers, so if the landing step waited, the blockers would hit their guard and say so. The
/// guard only ends the test instead of hanging it; the assertion is on the flag, not on a time.
#[test]
fn a_search_the_pool_has_not_run_lands_on_time_anyway() {
    let pool = AsyncComputePool::get();
    let threads = tasks::sizes().map_or(1, |sizes| sizes.async_compute);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let timed_out = Arc::new(AtomicBool::new(false));
    for _ in 0..threads {
        let (gate, started, timed_out) = (Arc::clone(&gate), Arc::clone(&started), Arc::clone(&timed_out));
        pool.spawn(async move {
            started.fetch_add(1, Ordering::SeqCst);
            let (open, signal) = &*gate;
            let guard = open.lock().expect("lock");
            let (_guard, wait) = signal
                .wait_timeout_while(guard, Duration::from_secs(120), |open| !*open)
                .expect("lock");
            if wait.timed_out() {
                timed_out.store(true, Ordering::SeqCst);
            }
        })
        .detach();
    }
    while started.load(Ordering::SeqCst) < threads {
        std::thread::yield_now();
    }

    let planner = Arc::new(Planner::new(vec![
        Action::new("gather", 1.0).sets(0, true),
        Action::new("craft", 1.0).requires(0, true).sets(1, true),
    ]));
    let mut facts = FactTable::new();
    facts.add_bool("has_wood").expect("fact");
    facts.add_bool("has_table").expect("fact");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, GoapPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    let agent = app
        .world_mut()
        .spawn((
            GoapAgent::new(planner, facts, Goal::new().require(1, true)),
            Blackboard::new(),
        ))
        .id();
    while app.world().resource::<GoapStats>().tick < 2 {
        app.update();
    }
    let status = app.world().get::<GoapStatus>(agent).expect("status").clone();
    assert_eq!(status.state, GoapState::Executing, "landed on its step");
    assert_eq!(status.steps, ["gather", "craft"]);
    assert_eq!(app.world().resource::<GoapStats>().searches_run_inline, 1);
    assert!(!timed_out.load(Ordering::SeqCst), "the step waited on the pool");

    let (open, signal) = &*gate;
    *open.lock().expect("lock") = true;
    signal.notify_all();
}
