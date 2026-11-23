// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3),
// src/experimental/coroutines.rs. Copyright (c) 2019-2021 Fedor Logachev <not.fl3@gmail.com>,
// dual-licensed MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! Gameplay coroutines (`docs/specs/14-scene-and-framework.md` § B6): sequences that span frames,
//! written as `async` blocks.
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_framework::coroutine::{CoroutinePlugin, start_coroutine};
//!
//! #[derive(Resource, Default)]
//! struct Wave(u32);
//!
//! let mut app = App::new();
//! app.add_plugins((MinimalPlugins, CoroutinePlugin)).init_resource::<Wave>();
//! let owner = app.world_mut().spawn(()).id();
//! start_coroutine(app.world_mut(), Some(owner), |co| async move {
//!     for _ in 0..3 {
//!         co.queue(|world| world.resource_mut::<Wave>().0 += 1); // spawn a wave
//!         co.wait_frames(2).await;
//!     }
//!     co.wait_until(|world| world.resource::<Wave>().0 >= 3).await;
//! });
//! for _ in 0..6 {
//!     app.update();
//! }
//! assert_eq!(app.world().resource::<Wave>().0, 3);
//! ```
//!
//! # Model
//!
//! A coroutine is a future polled once per application frame by the [`CoroutinePlugin`] executor,
//! an exclusive system in [`Update`] before the script pass. A second exclusive executor in
//! [`FixedUpdate`] polls only coroutines waiting on game seconds, after timers and before the fixed
//! script pass. There is no waker: every live coroutine is polled every application frame (the
//! macroquad design). Coroutines live in generational storage inside the [`Coroutines`] resource,
//! so a [`Coroutine`] handle to a finished coroutine never reaches a newer one that reuses its slot.
//!
//! The future gets a [`Co`] context instead of the world. **The world is never borrowed across an
//! `.await`:** [`Co::queue`] and [`Co::with_world`] hand closures to the executor, which runs them
//! with `&mut World` right after the poll step that queued them, and [`Co::wait_until`] hands it a
//! predicate the executor evaluates with `&World`. Waits:
//!
//! - [`Co::wait_seconds`]: game seconds accumulated one [`FixedUpdate`] step at a time ([`Time`],
//!   so pausing and time scale apply);
//! - [`Co::wait_frames`] / [`Co::next_frame`]: frames the game runs unpaused ([`Co::game_frame`]):
//!   a frame with `Time::paused` set does not count, so a frame wait holds through a pause like
//!   everything else in the game world (ADR-10105);
//! - [`Co::wait_until`]: until a predicate over the world holds (checked once per frame, and at
//!   once when the wait starts);
//! - [`Co::with_world`]: runs a closure on the world and resumes in the same frame with its result.
//!
//! # Order
//!
//! Coroutines are polled in the order they were started (not slot order). A coroutine runs up to
//! its first `.await` on the first executor pass after it is started; one started from inside the
//! executor (from a queued closure) first runs on the next frame. Within one frame a coroutine is
//! re-polled only when a [`Co::with_world`] result is ready or a just-registered
//! [`Co::wait_until`] already holds, at most [`MAX_STEPS_PER_FRAME`] times.
//!
//! # Ownership and cancellation
//!
//! Like timers, coroutines are owner-bound: one started with `Some(owner)` is dropped (cancelled
//! at its current `.await`, running the destructors of its locals) on the first frame its owner no
//! longer exists. [`Coroutines::stop`] and [`Coroutines::stop_owned`] cancel explicitly.
//!
//! # Not saved, not reloaded
//!
//! A coroutine is a compiled state machine, not data: **coroutines are not saved by
//! [`save_game`](crate::save_game) and do not survive a game-library hot reload** (the same rule
//! as timer callbacks, `specs/20` § 4). Keep anything a save must restore in reflected components
//! or resources, and restart the sequence from that state after a load (for example from a
//! script's `start`).
//!
//! Ported from macroquad's `experimental::coroutines` (MIT/Apache-2.0, attributed above): the
//! executor model, the generational storage and the typed handle with its result retrieval. The
//! [`Co`] context, owner binding, start-order polling and the world-access queue are Regia Engine's.

use std::any::{Any, TypeId};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use regia_app::prelude::*;

mod generational_storage;
mod wait;

pub use generational_storage::GenerationalId;
pub use wait::{WaitFrames, WaitSeconds, WaitUntil, WithWorld};

use generational_storage::GenerationalStorage;

/// Most poll steps one coroutine gets in one frame (see the module docs, "Order"). A coroutine that
/// keeps resuming itself in the same frame is parked until the next frame at this cap.
pub const MAX_STEPS_PER_FRAME: u32 = 64;

type BoxedFuture = Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send>>;
type WorldCommand = Box<dyn FnOnce(&mut World) + Send>;
type Predicate = Box<dyn FnMut(&World) -> bool + Send>;

/// What a coroutine is waiting on right now (`coroutines.list`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum WaitingOn {
    /// Not started yet, or between awaits.
    #[default]
    Nothing,
    /// Game seconds left.
    Seconds(f32),
    /// Frames left.
    Frames(u32),
    /// A [`Co::wait_until`] predicate.
    Condition,
    /// A [`Co::with_world`] closure.
    World,
}

impl WaitingOn {
    /// A short name for diagnostics: `nothing`, `seconds`, `frames`, `condition` or `world`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Seconds(_) => "seconds",
            Self::Frames(_) => "frames",
            Self::Condition => "condition",
            Self::World => "world",
        }
    }
}

/// A registered [`Co::wait_until`] predicate.
struct Condition {
    key: u64,
    predicate: Predicate,
    met: bool,
}

/// State shared between a coroutine's [`Co`] handles and the executor.
#[derive(Default)]
struct CoShared {
    delta: f32,
    frame: u64,
    game_frame: u64,
    tick: u64,
    owner: Option<Entity>,
    commands: Vec<WorldCommand>,
    conditions: Vec<Condition>,
    results: Vec<(u64, Box<dyn Any + Send>)>,
    next_key: u64,
    /// A [`Co::with_world`] result arrived: poll again this frame.
    repoll: bool,
    waiting: WaitingOn,
}

/// The context a coroutine receives: frame time, waits and world access through the executor.
///
/// Cheap to clone; clones share one coroutine's queue. See the [module docs](self).
#[derive(Clone, Default)]
pub struct Co {
    shared: Arc<Mutex<CoShared>>,
}

impl Co {
    fn lock(&self) -> MutexGuard<'_, CoShared> {
        // A panic inside a queued closure poisons nothing we rely on: the data stays consistent.
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Game seconds of the current frame ([`Time::delta_secs`]; zero while paused).
    pub fn delta_secs(&self) -> f32 {
        self.lock().delta
    }

    /// The current frame number ([`Time::frame_count`]).
    pub fn frame(&self) -> u64 {
        self.lock().frame
    }

    /// Frames run while the game was not paused ([`Time::paused`] off): what
    /// [`Self::wait_frames`] and [`Self::next_frame`] count (ADR-10105).
    pub fn game_frame(&self) -> u64 {
        self.lock().game_frame
    }

    /// The entity this coroutine is bound to, if any.
    pub fn owner(&self) -> Option<Entity> {
        self.lock().owner
    }

    /// Resumes after `secs` game seconds. Starts counting on the next fixed step, so
    /// `wait_seconds(0.5)` at 10 fixed steps per second resumes five steps later; zero or negative
    /// resumes at once.
    pub fn wait_seconds(&self, secs: f32) -> WaitSeconds {
        WaitSeconds::new(self.clone(), secs)
    }

    /// Resumes `frames` unpaused frames later (`0` resumes at once). A frame with [`Time::paused`]
    /// set does not count ([`Self::game_frame`], ADR-10105).
    pub fn wait_frames(&self, frames: u32) -> WaitFrames {
        WaitFrames::new(self.clone(), frames)
    }

    /// Resumes on the next frame (`wait_frames(1)`).
    pub fn next_frame(&self) -> WaitFrames {
        self.wait_frames(1)
    }

    /// Resumes once `predicate` holds. The executor evaluates it with `&World` when the wait starts
    /// (after this step's queued closures ran) and then once per frame before the coroutine is
    /// polled.
    pub fn wait_until(&self, predicate: impl FnMut(&World) -> bool + Send + 'static) -> WaitUntil {
        WaitUntil::new(self.clone(), Box::new(predicate))
    }

    /// Runs `f` with `&mut World` right after this poll step and resumes in the same frame with its
    /// result.
    pub fn with_world<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut World) -> T + Send + 'static,
    ) -> WithWorld<T> {
        WithWorld::new(self.clone(), Box::new(f))
    }

    /// Queues `f` to run with `&mut World` right after this poll step (fire and forget; closures run
    /// in queue order).
    pub fn queue(&self, f: impl FnOnce(&mut World) + Send + 'static) {
        self.lock().commands.push(Box::new(f));
    }

    fn next_key(&self) -> u64 {
        let mut shared = self.lock();
        shared.next_key += 1;
        shared.next_key
    }

    fn set_waiting(&self, waiting: WaitingOn) {
        self.lock().waiting = waiting;
    }

    fn begin_pass(&self, delta: f32, frame: u64, game_frame: u64, tick: u64) {
        let mut shared = self.lock();
        shared.delta = delta;
        shared.frame = frame;
        shared.game_frame = game_frame;
        shared.tick = tick;
        shared.repoll = false;
    }

    /// Runs queued closures (including ones they queue) until the queue is empty.
    fn apply_commands(&self, world: &mut World) {
        loop {
            let commands = std::mem::take(&mut self.lock().commands);
            if commands.is_empty() {
                return;
            }
            for command in commands {
                command(world);
            }
        }
    }

    /// Evaluates the unmet predicates; true when one became met. The lock is released while the
    /// predicates run.
    fn evaluate_conditions(&self, world: &World) -> bool {
        let mut conditions = std::mem::take(&mut self.lock().conditions);
        let mut newly_met = false;
        for condition in conditions.iter_mut().filter(|c| !c.met) {
            if (condition.predicate)(world) {
                condition.met = true;
                newly_met = true;
            }
        }
        let mut shared = self.lock();
        conditions.append(&mut shared.conditions);
        shared.conditions = conditions;
        newly_met
    }

    fn take_repoll(&self) -> bool {
        std::mem::take(&mut self.lock().repoll)
    }
}

/// A coroutine's future or its untaken result (`()` coroutines free their slot instead).
enum CoroutineState {
    Running(BoxedFuture),
    /// Taken out by the executor for the duration of its poll step.
    Polling,
    Value(Box<dyn Any + Send>),
}

struct CoroutineSlot {
    /// `Mutex` only to make the resource `Sync`; the executor has exclusive access.
    state: Mutex<CoroutineState>,
    /// `false` for `()` coroutines: they free their slot as soon as they finish.
    has_value: bool,
    owner: Option<Entity>,
    label: String,
    co: Co,
    started_frame: u64,
}

impl CoroutineSlot {
    fn state(&mut self) -> &mut CoroutineState {
        self.state.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Typed handle to a started coroutine. `T` is its output, retrieved with
/// [`Coroutines::take_result`].
pub struct Coroutine<T = ()> {
    id: GenerationalId,
    _output: PhantomData<fn() -> T>,
}

impl<T> Clone for Coroutine<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Coroutine<T> {}

impl<T> std::fmt::Debug for Coroutine<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coroutine").field("id", &self.id).finish()
    }
}

impl<T> PartialEq for Coroutine<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Coroutine<T> {
    /// The storage id (diagnostics).
    pub fn id(&self) -> GenerationalId {
        self.id
    }
}

/// One row of [`Coroutines::list`].
#[derive(Debug, Clone, PartialEq)]
pub struct CoroutineInfo {
    /// Storage id.
    pub id: GenerationalId,
    /// The label given at start (empty when none).
    pub label: String,
    /// The owner entity, if bound.
    pub owner: Option<Entity>,
    /// Frame it was started on.
    pub started_frame: u64,
    /// What it is waiting on.
    pub waiting: WaitingOn,
    /// Finished with a value nobody has taken yet.
    pub finished: bool,
}

/// Every live coroutine, in start order. Start them with [`start_coroutine`], [`Coroutines::start`]
/// or `ScriptCtx::start_coroutine` (owner-bound to the script's entity).
#[derive(Resource, Default)]
pub struct Coroutines {
    storage: GenerationalStorage<CoroutineSlot>,
    /// Ids in start order; the executor polls in this order.
    order: Vec<GenerationalId>,
    /// The frame number as of the last executor pass (start bookkeeping only).
    frame: u64,
    /// Unpaused frames counted by the `Update` passes (frame waits, ADR-10105).
    game_frame: u64,
    /// The frame number the game-frame count last looked at, so a frame counts once.
    game_frame_seen: Option<u64>,
    /// Monotonic fixed-step executor pass; unlike the frame number, this advances during a hitch.
    tick: u64,
}

impl Coroutines {
    /// Starts a coroutine. `body` receives the coroutine's [`Co`] and returns its future, e.g.
    /// `|co| async move { co.wait_seconds(1.0).await; 42 }`. With `Some(owner)` it is cancelled
    /// when that entity is despawned.
    pub fn start<T, F, Fut>(&mut self, owner: Option<Entity>, body: F) -> Coroutine<T>
    where
        T: Send + 'static,
        F: FnOnce(Co) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        self.start_labeled("", owner, body)
    }

    /// [`Coroutines::start`] with a label shown by `coroutines.list`.
    pub fn start_labeled<T, F, Fut>(
        &mut self,
        label: impl Into<String>,
        owner: Option<Entity>,
        body: F,
    ) -> Coroutine<T>
    where
        T: Send + 'static,
        F: FnOnce(Co) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let co = Co::default();
        co.lock().owner = owner;
        let future = body(co.clone());
        let future: BoxedFuture = Box::pin(async move { Box::new(future.await) as Box<dyn Any + Send> });
        let id = self.storage.push(CoroutineSlot {
            state: Mutex::new(CoroutineState::Running(future)),
            has_value: TypeId::of::<T>() != TypeId::of::<()>(),
            owner,
            label: label.into(),
            co,
            started_frame: self.frame,
        });
        self.order.push(id);
        Coroutine {
            id,
            _output: PhantomData,
        }
    }

    /// Whether the coroutine finished or was stopped.
    pub fn is_done<T>(&mut self, coroutine: Coroutine<T>) -> bool {
        self.storage
            .get_mut(coroutine.id)
            .is_none_or(|slot| matches!(slot.state(), CoroutineState::Value(_)))
    }

    /// Takes the output of a finished coroutine (once) and frees its slot; `None` while it runs,
    /// after it was stopped, or when already taken. `()` coroutines keep no result: they return
    /// `Some(())` whenever [`Coroutines::is_done`] holds (macroquad's rule).
    pub fn take_result<T: 'static>(&mut self, coroutine: Coroutine<T>) -> Option<T> {
        if TypeId::of::<T>() == TypeId::of::<()>() {
            if !self.is_done(coroutine) {
                return None;
            }
            let unit: Box<dyn Any> = Box::new(());
            return unit.downcast::<T>().ok().map(|value| *value);
        }
        let slot = self.storage.get_mut(coroutine.id)?;
        if !matches!(slot.state(), CoroutineState::Value(_)) {
            return None;
        }
        let slot = self.storage.free(coroutine.id)?;
        match slot.state.into_inner().unwrap_or_else(PoisonError::into_inner) {
            CoroutineState::Value(value) => value.downcast::<T>().ok().map(|value| *value),
            _ => None,
        }
    }

    /// Cancels a coroutine (drops its future at its current `.await`).
    pub fn stop<T>(&mut self, coroutine: Coroutine<T>) {
        self.storage.free(coroutine.id);
    }

    /// Cancels every coroutine bound to `owner`.
    pub fn stop_owned(&mut self, owner: Entity) {
        for id in self.order.clone() {
            if self.storage.get(id).is_some_and(|slot| slot.owner == Some(owner)) {
                self.storage.free(id);
            }
        }
        self.prune();
    }

    /// Cancels every coroutine.
    pub fn stop_all(&mut self) {
        self.storage.clear();
        self.order.clear();
    }

    /// Number of coroutines still running.
    pub fn len(&self) -> usize {
        self.order
            .iter()
            .filter(|id| {
                self.storage.get(**id).is_some_and(|slot| {
                    // A shared lock is fine here: nothing else holds it outside a poll step.
                    !matches!(
                        *slot.state.lock().unwrap_or_else(PoisonError::into_inner),
                        CoroutineState::Value(_)
                    )
                })
            })
            .count()
    }

    /// Whether no coroutine is running.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every coroutine that is running or holds an untaken result, in start order.
    pub fn list(&self) -> Vec<CoroutineInfo> {
        self.order
            .iter()
            .filter_map(|id| {
                let slot = self.storage.get(*id)?;
                let finished = matches!(
                    *slot.state.lock().unwrap_or_else(PoisonError::into_inner),
                    CoroutineState::Value(_)
                );
                Some(CoroutineInfo {
                    id: *id,
                    label: slot.label.clone(),
                    owner: slot.owner,
                    started_frame: slot.started_frame,
                    waiting: slot.co.lock().waiting,
                    finished,
                })
            })
            .collect()
    }

    /// Bytes reserved for coroutine slots (excluding the futures themselves).
    pub fn allocated_memory(&self) -> usize {
        self.storage.allocated_memory()
    }

    /// Drops ids of freed slots from the start order.
    fn prune(&mut self) {
        let storage = &self.storage;
        self.order.retain(|id| storage.get(*id).is_some());
    }
}

/// Starts a coroutine ([`Coroutines::start`]), inserting the resource when absent so a missing
/// [`CoroutinePlugin`] never panics user code (the coroutine then waits until the plugin runs).
pub fn start_coroutine<T, F, Fut>(world: &mut World, owner: Option<Entity>, body: F) -> Coroutine<T>
where
    T: Send + 'static,
    F: FnOnce(Co) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
{
    world
        .get_resource_or_insert_with(Coroutines::default)
        .start(owner, body)
}

/// The executor's outcome for one coroutine in one frame.
enum Stepped {
    Pending(BoxedFuture),
    Finished(Box<dyn Any + Send>),
}

/// Polls one coroutine for this frame: evaluate its conditions, poll, run its queued closures,
/// and re-poll while it can make progress in the same frame.
fn step_coroutine(world: &mut World, co: &Co, mut future: BoxedFuture, label: &str) -> Stepped {
    let mut context = Context::from_waker(Waker::noop());
    co.evaluate_conditions(world);
    for _ in 0..MAX_STEPS_PER_FRAME {
        let poll = future.as_mut().poll(&mut context);
        co.apply_commands(world);
        if let Poll::Ready(value) = poll {
            co.set_waiting(WaitingOn::Nothing);
            return Stepped::Finished(value);
        }
        let repoll = co.take_repoll() | co.evaluate_conditions(world);
        if !repoll {
            return Stepped::Pending(future);
        }
    }
    tracing::warn!(
        target: "regia_framework",
        "coroutine {label:?} resumed itself {MAX_STEPS_PER_FRAME} times in one frame; parked until the next frame"
    );
    Stepped::Pending(future)
}

/// `Update` (before the script pass): polls every coroutine once, in start order, and cancels the
/// ones whose owner is gone. This pass never advances a second-based wait's accumulated time.
///
/// Exclusive. Each coroutine's future is taken out of [`Coroutines`] while it runs, so queued
/// closures may start or stop coroutines (including the running one) through the resource.
#[tracing::instrument(skip_all, target = "regia_framework", name = "coroutines_tick")]
pub(crate) fn run_coroutines(world: &mut World) {
    run_coroutines_inner(world, false);
}

/// `FixedUpdate` (after timers, before the fixed script pass): advances only second-based waits.
/// Frame waits, conditions and coroutines that have not had their initial `Update` poll stay there.
#[tracing::instrument(skip_all, target = "regia_framework", name = "coroutine_seconds_tick")]
fn run_coroutine_seconds(world: &mut World) {
    run_coroutines_inner(world, true);
}

fn run_coroutines_inner(world: &mut World, seconds_only: bool) {
    let (delta, frame, paused) = world.get_resource::<Time>().map_or((0.0, 0, false), |time| {
        (time.delta_secs(), time.frame_count(), time.paused)
    });
    let Some(mut hub) = world.get_resource_mut::<Coroutines>() else {
        return;
    };
    hub.frame = frame;
    // A paused frame does not count for frame waits (ADR-10105); each frame counts once.
    if !seconds_only && hub.game_frame_seen != Some(frame) {
        hub.game_frame_seen = Some(frame);
        if !paused {
            hub.game_frame = hub.game_frame.saturating_add(1);
        }
    }
    let game_frame = hub.game_frame;
    if seconds_only {
        hub.tick = hub.tick.saturating_add(1);
    }
    let tick = hub.tick;
    hub.prune();
    let order = hub.order.clone();

    for id in order {
        // Owner-bound: cancelled on despawn. The fixed pass skips every non-seconds wait so its
        // frame and condition semantics stay identical to the old application-frame executor.
        let Some((owner, waiting)) = world
            .get_resource::<Coroutines>()
            .and_then(|hub| hub.storage.get(id))
            .map(|slot| (slot.owner, slot.co.lock().waiting))
        else {
            continue;
        };
        if seconds_only && !matches!(waiting, WaitingOn::Seconds(_)) {
            continue;
        }
        if let Some(owner) = owner
            && world.get_entity(owner).is_err()
        {
            if let Some(mut hub) = world.get_resource_mut::<Coroutines>() {
                hub.storage.free(id);
            }
            continue;
        }

        let taken = {
            let Some(mut hub) = world.get_resource_mut::<Coroutines>() else {
                return;
            };
            let Some(slot) = hub.storage.get_mut(id) else {
                continue;
            };
            match std::mem::replace(slot.state(), CoroutineState::Polling) {
                CoroutineState::Running(future) => Some((future, slot.co.clone(), slot.label.clone())),
                other => {
                    *slot.state() = other;
                    None
                }
            }
        };
        let Some((future, co, label)) = taken else {
            continue;
        };

        co.begin_pass(delta, frame, game_frame, tick);
        let stepped = step_coroutine(world, &co, future, &label);

        // Put the future (or result) back, unless the coroutine was stopped while it ran.
        let Some(mut hub) = world.get_resource_mut::<Coroutines>() else {
            return;
        };
        let Some(slot) = hub.storage.get_mut(id) else {
            continue;
        };
        match stepped {
            Stepped::Pending(future) => *slot.state() = CoroutineState::Running(future),
            Stepped::Finished(value) if slot.has_value => *slot.state() = CoroutineState::Value(value),
            Stepped::Finished(_) => {
                hub.storage.free(id);
            }
        }
    }

    if let Some(mut hub) = world.get_resource_mut::<Coroutines>() {
        hub.prune();
    }
}

/// System set of the coroutine executor, for ordering game systems around it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CoroutineSet;

/// Registers [`Coroutines`], polls them every [`Update`] before the script pass, and advances
/// second-based waits every [`FixedUpdate`] after timers and before the fixed script pass.
pub struct CoroutinePlugin;

impl Plugin for CoroutinePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Coroutines>()
            .add_systems(
                Update,
                run_coroutines
                    .in_set(CoroutineSet)
                    .before(crate::script::ScriptsSet::Update),
            )
            .add_systems(
                FixedUpdate,
                run_coroutine_seconds
                    .in_set(CoroutineSet)
                    .after(crate::timers::tick_timers)
                    .before(crate::script::ScriptsSet::FixedUpdate),
            );
    }
}

#[cfg(test)]
mod tests;
