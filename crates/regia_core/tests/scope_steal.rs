// SPDX-License-Identifier: MPL-2.0
//! ADR-303: a thread waiting in a compute-pool scope runs other queued pool tasks.
//!
//! This is the pool behaviour behind the v0.1.26 boot hang: the waiter can run an unrelated
//! task (a `Send` system) on its own thread, so a lock the waiter holds across
//! `tasks::scope` / `tasks::par_chunks_mut` is a lock that task can never take. Fan-out code
//! takes what it needs under the lock and releases it first; this test pins the stealing and
//! the snapshot-then-fan-out pattern. Its own binary, so the compute pool has one thread.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};
use std::time::Duration;

use regia_core::tasks::{self, ComputePool, TaskPoolOptions};

/// Pending once (waking itself), then ready: lets the executor run the other queued tasks.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Holds the pool's only thread, steals, and returns (thread the stolen task ran on, this
/// thread, the value the fan-out read).
fn run() -> (std::thread::ThreadId, std::thread::ThreadId, u32) {
    // Occupy the pool's only thread, so only a scope waiter can run what is queued next.
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let blocker = ComputePool::get().spawn(async move {
        let _ = started_tx.send(());
        let _ = release_rx.recv();
    });
    started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the pool thread started the blocker");

    // An unrelated task that takes the lock the fan-out reads (a system querying the surface).
    let surface = Arc::new(Mutex::new(7u32));
    let ran = Arc::new(AtomicBool::new(false));
    let stolen = {
        let (surface, ran) = (Arc::clone(&surface), Arc::clone(&ran));
        ComputePool::get().spawn(async move {
            *surface.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            ran.store(true, Ordering::Release);
            std::thread::current().id()
        })
    };

    // The fixed pattern: read under the lock, release, then fan out. Holding the guard across
    // the scope instead would hang here: the waiter runs `stolen`, which waits on the guard.
    let value = *surface.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let ran_ref = &ran;
    let read = tasks::scope(|scope| {
        scope.spawn(async move {
            // Finish only after the unrelated task ran: the waiter must have stolen it.
            while !ran_ref.load(Ordering::Acquire) {
                YieldOnce(false).await;
            }
            value
        });
    });
    let stolen_on = tasks::block_on(stolen);
    let _ = release_tx.send(());
    tasks::block_on(blocker);
    assert_eq!(
        *surface.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        8
    );
    (stolen_on, std::thread::current().id(), read[0])
}

#[test]
fn a_scope_waiter_runs_unrelated_pool_tasks_on_its_own_thread() {
    tasks::init(TaskPoolOptions {
        compute_threads: Some(1),
        ..TaskPoolOptions::default()
    });
    assert_eq!(ComputePool::get().thread_num(), 1);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run());
    });
    // A failure guard only: the whole test takes milliseconds.
    let (stolen_on, waiter, read) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the scope waiter did not run the queued task (deadlock)");
    assert_eq!(
        stolen_on, waiter,
        "the scope waiter ran the unrelated task itself"
    );
    assert_eq!(read, 7);
}
