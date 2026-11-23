//! Spike game. `generated.rs` is rewritten by the host before every rebuild (variant A/B: a field
//! added, a field removed, a system renamed) — it stands in for the developer's edit.
//!
//! The game owns an external simulation thread (specs/25 § 3), in one of two arrangements:
//! * (a) default: the sim code is linked into this (reloaded) library; `on_unload` stops and joins
//!   the thread and saves its state into a reflected resource; `on_load` restarts it from there.
//! * (b) feature `sim-persistent`: the sim code lives in `sim_dylib` (never reloaded) and its handle
//!   in the engine's `PersistentStore`; the thread keeps running through reloads.

use spike_regia::hot::{GamePlugin, GameRegistrar};
use spike_regia::prelude::*;

#[cfg(feature = "sim-persistent")]
#[allow(unused_imports)]
use sim_dylib as _;

mod generated;
pub use generated::*;

/// Entities spawned at startup.
pub const ENTITIES: usize = 5_000;
/// Sim rate (Hz) on the sim's own clock.
pub const SIM_HZ: u32 = 120;
/// Discs in the sim.
pub const SIM_DISCS: usize = 256;

/// A component that does not change between generations.
#[derive(Component, Reflect, Default, Debug)]
#[reflect(Component, Default)]
pub struct Spin {
    /// Angle.
    pub angle: f32,
}

/// Frame counter + which generation's system last ran.
#[derive(Resource, Reflect, Default, Debug)]
#[reflect(Resource, Default)]
pub struct Score {
    /// Frames counted by game systems (survives reloads).
    pub frames: u64,
    /// `GEN` of the last generation whose system ran.
    pub gen_seen: u32,
}

/// Presentation view of the sim + its saved state for option (a).
#[derive(Resource, Reflect, Default, Debug)]
#[reflect(Resource, Default)]
pub struct SimView {
    /// Last sim tick the presentation saw.
    pub tick: u64,
    /// Checksum at that tick.
    pub checksum: u64,
    /// Option (a): serialized sim state written by `on_unload`.
    pub saved: Vec<u8>,
}

/// Option (a): the running sim thread (not reflected — cleaned up in `on_unload`).
#[derive(Resource)]
pub struct SimRunner(pub sim::SimHandle);

#[cfg(feature = "sim-persistent")]
const SIM_KEY: &str = "hover_sim";

fn spawn(mut commands: Commands) {
    for i in 0..ENTITIES {
        commands.spawn((Mover::default(), Spin { angle: i as f32 }));
    }
}

fn count_frames(mut score: ResMut<Score>, mut q: Query<&mut Spin>) {
    score.frames += 1;
    for mut s in &mut q {
        s.angle += 0.01;
    }
}

#[cfg(not(feature = "sim-persistent"))]
fn read_sim(runner: Option<Res<SimRunner>>, mut view: ResMut<SimView>) {
    if let Some(r) = runner {
        view.tick = r.0.tick();
        view.checksum = r.0.checksum();
    }
}

#[cfg(feature = "sim-persistent")]
fn read_sim(mut store: ResMut<PersistentStore>, mut view: ResMut<SimView>) {
    if let Some(h) = store.get_mut::<sim::SimHandle>(SIM_KEY) {
        view.tick = h.tick();
        view.checksum = h.checksum();
    }
}

/// The game plugin.
pub struct Game;

impl GamePlugin for Game {
    fn name(&self) -> &'static str {
        "spike_game"
    }

    fn register(&self, reg: &mut GameRegistrar<'_>) {
        reg.component::<Spin>().resource::<Score>().resource::<SimView>();
        generated::register(reg);
        reg.add_systems(Startup, spawn);
        reg.add_systems(Update, (count_frames, read_sim));
    }

    fn on_load(&self, world: &mut World) {
        #[cfg(not(feature = "sim-persistent"))]
        {
            let saved = std::mem::take(&mut world.resource_mut::<SimView>().saved);
            let state = sim::HoverSim::from_bytes(&saved).unwrap_or_else(|| sim::HoverSim::new(SIM_DISCS));
            world.insert_resource(SimRunner(sim::SimHandle::start(state, SIM_HZ)));
        }
        #[cfg(feature = "sim-persistent")]
        {
            world
                .get_resource_or_init::<PersistentStore>()
                .get_or_insert_with(SIM_KEY, || {
                    sim::SimHandle::start(sim::HoverSim::new(SIM_DISCS), SIM_HZ)
                });
        }
    }

    fn on_unload(&self, world: &mut World) {
        #[cfg(not(feature = "sim-persistent"))]
        if let Some(SimRunner(handle)) = world.remove_resource::<SimRunner>()
            && let Some(state) = handle.stop_join()
        {
            let mut view = world.resource_mut::<SimView>();
            view.tick = state.tick;
            view.checksum = state.checksum();
            view.saved = state.to_bytes();
        }
        #[cfg(feature = "sim-persistent")]
        let _ = world;
    }
}

spike_regia::export_game!(Game);
