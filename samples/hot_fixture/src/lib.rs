// SPDX-License-Identifier: MIT-0
//! Hot-reload game plugin (specs/20 §§ 2–4).
//!
//! The editor rebuilds this crate as a dylib:
//!
//! ```text
//! cargo rustc -p hot_fixture --lib --crate-type dylib --features hot-reload \
//!     -- -C metadata=hotgen<N> -C prefer-dynamic
//! ```
//!
//! `REGIA_ABI_HASH` is the decimal hash the editor exported as `REGIA_ABI_HASH` at that
//! build. Without the variable the static keeps the framework's
//! [`regia_framework::abi_hash`], which the ignored library test still accepts.
//!
//! State that must survive a reload lives in [`Mover`], [`Score`] and [`SimClock`] (`Reflect +
//! Default`), and in the [`Blinker`] and [`Spinner`] scripts (rebuilt from the new library on
//! each reload, specs/20 § 3.9). The scripts write [`ScriptProbe`] each tick, so a test sees
//! which build's code ran. The background thread is joined in `on_unload` and started again from
//! [`SimClock::ticks`] in `on_load`. An external simulation does not belong in this library
//! (specs/25 § 3); the host keeps that handle as an engine-owned resource.
//!
//! [`FixtureGame`] implements `GamePlugin`; `export_game!` generates `regia_game_entry` (the
//! `#[repr(C)]` export table the editor copies) and `REGIA_ABI_HASH`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(not(feature = "hot-reload"))]
use regia_app::prelude::*;
#[cfg(feature = "hot-reload")]
use regia_dylib::prelude::*;
use regia_framework::script::{RegisterScriptExt, Script, ScriptCtx, ScriptSlot, Scripts};

/// The game plugin the hot-reload host loads (specs/20 § 2).
pub struct FixtureGame;

impl regia_framework::GamePlugin for FixtureGame {
    fn register(&self, app: &mut App) {
        register_game(app);
    }

    fn on_unload(&self, world: &mut World) {
        unload_game(world);
    }

    fn on_load(&self, world: &mut World) {
        load_game(world);
    }
}

regia_framework::export_game!(FixtureGame);

/// Reflected body the reload tests edit: add `heading`, retype `speed`, keep `tag`.
#[derive(Component, Reflect, Default, Clone, Debug)]
#[reflect(Component, Default)]
pub struct Mover {
    /// Units per frame. The reload test retypes this to `f64`.
    pub speed: f32,
    /// Increments once per frame. The reload test adds `heading` after this field.
    pub tag: u32,
}

/// Reflected score. `marker` changes when the system body changes; `hits` must survive reloads.
#[derive(Resource, Reflect, Default, Clone, Debug)]
#[reflect(Resource, Default)]
pub struct Score {
    /// Frames the system has run. Must survive a reload.
    pub hits: u32,
    /// Set by the system body. The reload test changes the assigned value.
    pub marker: u32,
}

/// Thread counter flushed in `on_unload` so the snapshot sees it, and the first-spawn flag.
#[derive(Resource, Reflect, Default, Clone, Debug)]
#[reflect(Resource, Default)]
pub struct SimClock {
    /// Ticks observed when the thread was last joined.
    pub ticks: u64,
    /// Set once the first [`Mover`] exists, so a later `on_load` does not spawn another.
    pub spawned: bool,
}

/// What the scripts did on their last tick. The reload test reads it to see which build's script
/// body ran and that the scripts' fields carried over.
#[derive(Resource, Reflect, Default, Clone, Debug)]
#[reflect(Resource, Default)]
pub struct ScriptProbe {
    /// [`Blinker::count`] after the last tick.
    pub blink_count: u32,
    /// [`Blinker::stamp`] after the last tick. The reload test changes the value the body sets.
    pub blink_stamp: u32,
    /// `Spinner::turns` after the last tick (the reload test removes the `Spinner` type).
    pub spin_turns: u32,
}

/// Script the reload test edits: the body sets `stamp`; `count` must survive reloads.
#[derive(regia_macros::Script, Reflect, Default, Clone, Debug)]
pub struct Blinker {
    /// Ticks so far. Must survive a reload.
    pub count: u32,
    /// Set by the body. The reload test changes the assigned value.
    pub stamp: u32,
}

impl Script for Blinker {
    fn update(&mut self, ctx: &mut ScriptCtx) {
        self.count = self.count.saturating_add(1);
        self.stamp = 1;
        let (count, stamp) = (self.count, self.stamp);
        if let Some(mut probe) = ctx.world().get_resource_mut::<ScriptProbe>() {
            probe.blink_count = count;
            probe.blink_stamp = stamp;
        }
    }
}

// spinner-begin
/// Script the reload test removes; its slot is then dropped with a warning.
#[derive(regia_macros::Script, Reflect, Default, Clone, Debug)]
pub struct Spinner {
    /// Ticks so far.
    pub turns: u32,
}

impl Script for Spinner {
    fn update(&mut self, ctx: &mut ScriptCtx) {
        self.turns = self.turns.saturating_add(1);
        let turns = self.turns;
        if let Some(mut probe) = ctx.world().get_resource_mut::<ScriptProbe>() {
            probe.spin_turns = turns;
        }
    }
}
// spinner-end

fn register_game(app: &mut App) {
    app.register_type::<Mover>()
        .register_type::<Score>()
        .register_type::<SimClock>()
        .register_type::<ScriptProbe>()
        .register_type::<Blinker>();
    app.init_resource::<Score>();
    app.init_resource::<SimClock>();
    app.init_resource::<ScriptProbe>();
    app.register_script::<Blinker>();
    // spinner-begin
    app.register_type::<Spinner>().register_script::<Spinner>();
    // spinner-end
    app.add_systems(Update, move_bodies);
}

fn unload_game(world: &mut World) {
    let ticks = stop_sim();
    if let Some(mut clock) = world.get_resource_mut::<SimClock>() {
        clock.ticks = ticks;
    }
}

// Assign after `Default` so an added field still compiles and a retyped `speed` accepts `4.5`.
#[allow(clippy::field_reassign_with_default)]
fn load_game(world: &mut World) {
    let (ticks, spawned) = world
        .get_resource::<SimClock>()
        .map(|clock| (clock.ticks, clock.spawned))
        .unwrap_or((0, false));
    if !spawned {
        let mut mover = Mover::default();
        mover.speed = 4.5;
        mover.tag = 9;
        world.spawn(mover);
        // The scripts sit on their own entity so `Mover` counts stay one.
        let slots = vec![
            ScriptSlot::new::<Blinker>(),
            // spinner-begin
            ScriptSlot::new::<Spinner>(),
            // spinner-end
        ];
        world.spawn(slots.into_iter().collect::<Scripts>());
        if let Some(mut clock) = world.get_resource_mut::<SimClock>() {
            clock.spawned = true;
        }
    }
    start_sim(ticks);
}

fn move_bodies(mut score: ResMut<Score>, mut bodies: Query<&mut Mover>) {
    score.hits = score.hits.saturating_add(1);
    score.marker = 1;
    for mut body in &mut bodies {
        body.tag = body.tag.saturating_add(1);
    }
}

struct SimRun {
    stop: std::sync::Arc<AtomicBool>,
    ticks: std::sync::Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

fn sim_slot() -> &'static Mutex<Option<SimRun>> {
    static SIM: OnceLock<Mutex<Option<SimRun>>> = OnceLock::new();
    SIM.get_or_init(|| Mutex::new(None))
}

fn lock_sim() -> std::sync::MutexGuard<'static, Option<SimRun>> {
    match sim_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn start_sim(from: u64) {
    let mut slot = lock_sim();
    if slot.is_some() {
        return;
    }
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let ticks = std::sync::Arc::new(AtomicU64::new(from));
    let stop_flag = std::sync::Arc::clone(&stop);
    let tick_flag = std::sync::Arc::clone(&ticks);
    let join = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            tick_flag.fetch_add(1, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(5));
        }
    });
    *slot = Some(SimRun {
        stop,
        ticks,
        join: Some(join),
    });
}

fn stop_sim() -> u64 {
    let mut slot = lock_sim();
    let Some(mut run) = slot.take() else {
        return 0;
    };
    run.stop.store(true, Ordering::Relaxed);
    if let Some(join) = run.join.take()
        && join.join().is_err()
    {
        eprintln!("hot_fixture: sim thread panicked");
    }
    run.ticks.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_table_is_three_pointers() {
        use regia_framework::GameExports;
        assert_eq!(size_of::<GameExports>(), size_of::<usize>() * 3);
        assert_eq!(align_of::<GameExports>(), align_of::<usize>());
        assert!(!regia_game_entry().is_null());
    }

    #[test]
    fn abi_hash_is_the_framework_value_for_this_build() {
        assert_eq!(
            REGIA_ABI_HASH,
            regia_framework::abi_hash_from(option_env!("REGIA_ABI_HASH"))
        );
    }

    #[test]
    fn register_load_and_unload_round_trip_in_process() {
        let mut app = App::new();
        app.set_current_owner(TypeOwner::Game(1));
        register_game(&mut app);
        app.set_current_owner(TypeOwner::Engine);
        load_game(app.world_mut());
        app.update();
        {
            let score = app.world().resource::<Score>();
            assert_eq!(score.hits, 1);
            assert_eq!(score.marker, 1);
            assert!(app.world().resource::<SimClock>().spawned);
        }
        let mut query = app.world_mut().query::<&Mover>();
        let movers = query.iter(app.world()).count();
        assert_eq!(movers, 1);
        let registered = app
            .world()
            .resource::<regia_framework::script::ScriptRegistry>()
            .iter()
            .count();
        assert_eq!(registered, 2, "Blinker and Spinner");
        let mut scripted = app.world_mut().query::<&Scripts>();
        let slots: usize = scripted.iter(app.world()).map(|s| s.0.len()).sum();
        assert_eq!(slots, 2);
        unload_game(app.world_mut());
        assert!(lock_sim().is_none(), "on_unload joins the thread");
    }
}
