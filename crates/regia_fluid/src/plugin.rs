// SPDX-License-Identifier: MPL-2.0
//! The ECS side: [`FluidWorld`] holds the simulations (and their caches) by id, [`FluidPlugin`]
//! steps the ones set to run on `FixedUpdate` and, with the `draw` feature, publishes each one's
//! liquid surface and whitewater points as `MeshRenderer` entities.
//!
//! Nothing is installed by default: an app adds `FluidPlugin` itself, so the engine's default
//! schedules (and the determinism-matrix traces) do not change.

use regia_app::prelude::*;

use crate::cache::FluidCache;
use crate::frame::FluidFrame;
use crate::sim::FluidSim;

/// Most simulations one world may hold.
pub const MAX_SIMS: usize = 16;

/// One simulation and its bake.
#[derive(Debug, Clone)]
pub struct FluidEntry {
    /// Stable id (`fluid.*` agent methods use it).
    pub id: u32,
    /// The preset it started from, if any.
    pub preset: Option<String>,
    /// The live solver.
    pub sim: FluidSim,
    /// The last bake.
    pub cache: Option<FluidCache>,
    /// Playback: the cache frame shown instead of the live state.
    pub playing: Option<usize>,
    /// Whether `FluidPlugin` steps it every `FixedUpdate`.
    pub running: bool,
    /// Lattice samples per cell edge of the published surface (0 = no surface).
    pub surface_resolution: u32,
    /// Bumped whenever the shown state changes (drawing re-publishes on a change).
    pub revision: u64,
}

impl FluidEntry {
    /// The frame to show: the cache frame while playing, else the live state. A playback index
    /// the cache cannot decode (past its end) shows the live state too, never an empty frame
    /// that would look like an empty recording; `cache.play` reports the error itself.
    #[must_use]
    pub fn shown_frame(&self) -> FluidFrame {
        match (self.playing, &self.cache) {
            (Some(i), Some(cache)) => cache.play(i).unwrap_or_else(|_| self.sim.frame()),
            _ => self.sim.frame(),
        }
    }
}

/// Every simulation of the app, in creation order.
#[derive(Resource, Debug, Clone, Default)]
pub struct FluidWorld {
    entries: Vec<FluidEntry>,
    next_id: u32,
}

impl FluidWorld {
    /// Adds `sim`; `None` at [`MAX_SIMS`].
    pub fn insert(&mut self, sim: FluidSim, preset: Option<String>) -> Option<u32> {
        if self.entries.len() >= MAX_SIMS {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(FluidEntry {
            id,
            preset,
            sim,
            cache: None,
            playing: None,
            running: false,
            surface_resolution: 2,
            revision: 0,
        });
        Some(id)
    }

    /// The entry with `id`.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&FluidEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The entry with `id`, mutably.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut FluidEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Removes `id`; whether it existed.
    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        before != self.entries.len()
    }

    /// Every entry.
    #[must_use]
    pub fn entries(&self) -> &[FluidEntry] {
        &self.entries
    }
}

/// The `FixedUpdate` set the fluid step runs in (the physics coupling orders around it).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FluidStep;

/// Steps running simulations on `FixedUpdate`; with `draw`, publishes them in `PostUpdate`.
#[derive(Debug, Default, Clone, Copy)]
pub struct FluidPlugin;

impl Plugin for FluidPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FluidWorld>()
            .add_systems(FixedUpdate, step_running_fluids.in_set(FluidStep));
        #[cfg(feature = "draw")]
        app.add_systems(PostUpdate, crate::draw::publish_fluids);
    }
}

/// Steps every running simulation once (playback entries are skipped).
pub fn step_running_fluids(mut world: ResMut<FluidWorld>) {
    let _span = tracing::info_span!(target: "regia_fluid", "fluid_fixed_update").entered();
    if !world.entries.iter().any(|e| e.running && e.playing.is_none()) {
        return;
    }
    for entry in &mut world.entries {
        if entry.running && entry.playing.is_none() {
            entry.sim.step();
            entry.revision += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use regia_ecs::ecs::system::RunSystemOnce;

    use super::*;
    use crate::presets;
    use crate::sim::FluidParams;

    fn splash() -> FluidSim {
        let (lo, hi, dx) = presets::default_bounds("splash").expect("bounds");
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx * 2.0,
            particle_cap: 2000,
            ..FluidParams::default()
        };
        presets::build("splash", params).expect("splash")
    }

    /// `step_running_fluids` steps the running entries only, and never one that is playing a cache.
    #[test]
    fn only_running_live_entries_step() {
        let mut world = World::new();
        let mut fluids = FluidWorld::default();
        let a = fluids.insert(splash(), Some("splash".into())).expect("a");
        let b = fluids.insert(splash(), None).expect("b");
        let c = fluids.insert(splash(), None).expect("c");
        for id in [a, c] {
            fluids.get_mut(id).expect("entry").running = true;
        }
        fluids.get_mut(c).expect("c").playing = Some(0);
        world.insert_resource(fluids);
        for _ in 0..3 {
            world.run_system_once(step_running_fluids).expect("system");
        }
        let fluids = world.resource::<FluidWorld>();
        let steps = |id| fluids.get(id).map(|e| e.sim.steps());
        assert_eq!((steps(a), steps(b), steps(c)), (Some(3), Some(0), Some(0)));
        assert_eq!(fluids.get(a).map(|e| e.revision), Some(3));
    }

    /// (Audit G-fluid #12) A playback index past the end of the bake shows the live state, not an
    /// empty frame that looks like an empty recording.
    #[test]
    fn playback_past_the_end_keeps_a_frame() {
        let mut sim = splash();
        sim.step();
        let mut cache = FluidCache::for_sim(&sim, false);
        cache.record(&sim).expect("record");
        let mut fluids = FluidWorld::default();
        let id = fluids.insert(sim, None).expect("id");
        let entry = fluids.get_mut(id).expect("entry");
        entry.cache = Some(cache);
        entry.playing = Some(5);
        let shown = entry.shown_frame();
        assert!(!shown.is_empty(), "past-the-end playback showed an empty frame");
        assert_eq!(shown, entry.sim.frame());
    }

    /// The world refuses a seventeenth simulation and forgets removed ids.
    #[test]
    fn world_caps_and_removes() {
        let mut fluids = FluidWorld::default();
        let sim = splash();
        let ids: Vec<u32> = (0..MAX_SIMS)
            .filter_map(|_| fluids.insert(sim.clone(), None))
            .collect();
        assert_eq!(ids.len(), MAX_SIMS);
        assert!(fluids.insert(sim.clone(), None).is_none());
        assert!(fluids.remove(ids[0]));
        assert!(!fluids.remove(ids[0]));
        assert!(fluids.get(ids[0]).is_none());
        assert!(fluids.insert(sim, None).is_some());
    }
}
