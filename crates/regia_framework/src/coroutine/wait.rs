// SPDX-License-Identifier: MPL-2.0
//! The futures a coroutine awaits ([`Co`](super::Co) builds them). None of them uses the waker:
//! the executor polls every coroutine each application frame, polls second-based waits on fixed
//! steps, and re-polls within a pass when a [`WithWorld`] result or a met [`WaitUntil`] predicate
//! is ready.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use regia_app::prelude::*;

use super::{Co, Condition, Predicate, WaitingOn};

/// Per-step slack when comparing accumulated fixed-step deltas with a wait's length. Scaling with
/// the delta absorbs `f32` step rounding without letting a 10 µs absolute epsilon finish a wait
/// early at very high fixed rates.
const SECONDS_EPSILON_PER_STEP: f64 = 1e-4;

/// Slack per counted second for the `f32` rounding of the fixed deltas (twice `f32`'s 2^-24 unit
/// roundoff).
const RELATIVE_STEP_ROUNDING: f64 = 1.0 / 8_388_608.0;

/// [`Co::wait_seconds`]: resumes after a number of game seconds.
#[must_use = "a wait does nothing unless awaited"]
pub struct WaitSeconds {
    co: Co,
    secs: f64,
    elapsed: f64,
    last_tick: Option<u64>,
}

impl WaitSeconds {
    pub(super) fn new(co: Co, secs: f32) -> Self {
        Self {
            co,
            secs: f64::from(secs),
            elapsed: 0.0,
            last_tick: None,
        }
    }
}

impl Future for WaitSeconds {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let (delta, tick) = {
            let shared = this.co.lock();
            (f64::from(shared.delta), shared.tick)
        };
        let mut advanced = false;
        match this.last_tick {
            // Counting starts on the next fixed step: the pass that started the wait is spent.
            None => this.last_tick = Some(tick),
            Some(last) if tick > last => {
                this.last_tick = Some(tick);
                this.elapsed += delta;
                advanced = true;
            }
            Some(_) => {}
        }
        // The `Update` pass has the presentation delta, not a fixed delta. Its larger value must
        // not enlarge the rounding slack and complete a wait outside the fixed-step executor.
        // Each fixed delta is an `f32`, off the true step by up to 2^-24 of it, so the rounding
        // the slack must absorb grows with the counted time: `elapsed · 2^-23` covers it with a
        // factor of two (a 100 s wait at 50 Hz falls 2.2 µs short over its 5000 steps, more than
        // the per-step 2 µs; corr-framework-a L1). Capped at half a step, so the slack never
        // finishes a wait a whole step early.
        let epsilon = if advanced {
            (delta * SECONDS_EPSILON_PER_STEP + this.elapsed * RELATIVE_STEP_ROUNDING).min(delta * 0.5)
        } else {
            0.0
        };
        if this.elapsed + epsilon >= this.secs {
            this.co.set_waiting(WaitingOn::Nothing);
            Poll::Ready(())
        } else {
            this.co
                .set_waiting(WaitingOn::Seconds((this.secs - this.elapsed) as f32));
            Poll::Pending
        }
    }
}

/// [`Co::wait_frames`] / [`Co::next_frame`]: resumes a number of frames later.
#[must_use = "a wait does nothing unless awaited"]
pub struct WaitFrames {
    co: Co,
    remaining: u32,
    last_frame: Option<u64>,
}

impl WaitFrames {
    pub(super) fn new(co: Co, frames: u32) -> Self {
        Self {
            co,
            remaining: frames,
            last_frame: None,
        }
    }
}

impl Future for WaitFrames {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let frame = this.co.game_frame();
        match this.last_frame {
            None => this.last_frame = Some(frame),
            Some(last) if frame > last => {
                this.last_frame = Some(frame);
                this.remaining = this.remaining.saturating_sub(1);
            }
            Some(_) => {}
        }
        if this.remaining == 0 {
            this.co.set_waiting(WaitingOn::Nothing);
            Poll::Ready(())
        } else {
            this.co.set_waiting(WaitingOn::Frames(this.remaining));
            Poll::Pending
        }
    }
}

/// [`Co::wait_until`]: resumes once a predicate over the world holds.
#[must_use = "a wait does nothing unless awaited"]
pub struct WaitUntil {
    co: Co,
    predicate: Option<Predicate>,
    key: Option<u64>,
}

impl WaitUntil {
    pub(super) fn new(co: Co, predicate: Predicate) -> Self {
        Self {
            co,
            predicate: Some(predicate),
            key: None,
        }
    }
}

impl Future for WaitUntil {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let Some(key) = this.key else {
            // First poll: hand the predicate to the executor, which evaluates it after this step.
            let Some(predicate) = this.predicate.take() else {
                return Poll::Ready(());
            };
            let key = this.co.next_key();
            this.key = Some(key);
            let mut shared = this.co.lock();
            shared.conditions.push(Condition {
                key,
                predicate,
                met: false,
            });
            shared.waiting = WaitingOn::Condition;
            return Poll::Pending;
        };
        let mut shared = this.co.lock();
        let Some(index) = shared.conditions.iter().position(|c| c.key == key) else {
            // Cannot happen (only this future removes its condition); do not hang if it does.
            return Poll::Ready(());
        };
        if shared.conditions[index].met {
            // Drop the condition (and what its closure captured) after the lock is released: user
            // state may call back into `Co`, which locks the same mutex.
            let removed = shared.conditions.swap_remove(index);
            shared.waiting = WaitingOn::Nothing;
            drop(shared);
            drop(removed);
            this.key = None;
            Poll::Ready(())
        } else {
            shared.waiting = WaitingOn::Condition;
            Poll::Pending
        }
    }
}

impl Drop for WaitUntil {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            // Dropped after the lock is released (see `poll`).
            let removed = {
                let mut shared = self.co.lock();
                let index = shared.conditions.iter().position(|c| c.key == key);
                index.map(|i| shared.conditions.remove(i))
            };
            drop(removed);
        }
    }
}

/// The closure a [`WithWorld`] runs.
type WorldFn<T> = Box<dyn FnOnce(&mut World) -> T + Send>;

/// [`Co::with_world`]: runs a closure on the world and resumes with its result in the same frame.
#[must_use = "with_world does nothing unless awaited"]
pub struct WithWorld<T> {
    co: Co,
    f: Option<WorldFn<T>>,
    key: Option<u64>,
    _output: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> WithWorld<T> {
    pub(super) fn new(co: Co, f: WorldFn<T>) -> Self {
        Self {
            co,
            f: Some(f),
            key: None,
            _output: PhantomData,
        }
    }
}

impl<T: Send + 'static> Future for WithWorld<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if let Some(f) = this.f.take() {
            let key = this.co.next_key();
            this.key = Some(key);
            let co = this.co.clone();
            this.co.queue(move |world| {
                let value = f(world);
                let mut shared = co.lock();
                shared.results.push((key, Box::new(value) as Box<dyn Any + Send>));
                shared.repoll = true;
            });
            this.co.set_waiting(WaitingOn::World);
            return Poll::Pending;
        }
        let Some(key) = this.key else {
            // Polled again after completion; a well-behaved caller never does.
            return Poll::Pending;
        };
        let mut shared = this.co.lock();
        let Some(index) = shared.results.iter().position(|(k, _)| *k == key) else {
            return Poll::Pending;
        };
        let (_, value) = shared.results.swap_remove(index);
        match value.downcast::<T>() {
            Ok(value) => {
                shared.waiting = WaitingOn::Nothing;
                drop(shared);
                this.key = None;
                Poll::Ready(*value)
            }
            // The key is unique to this future, so the type always matches.
            Err(_) => Poll::Pending,
        }
    }
}

impl<T> Drop for WithWorld<T> {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            // Dropped after the lock is released: the result may own user state that locks `Co`.
            let removed = {
                let mut shared = self.co.lock();
                let index = shared.results.iter().position(|(k, _)| *k == key);
                index.map(|i| shared.results.remove(i))
            };
            drop(removed);
        }
    }
}
