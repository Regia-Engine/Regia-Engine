// SPDX-License-Identifier: MPL-2.0
//! A recorded input log plus its start snapshot as a golden scenario (DC-3.7b).
//!
//! [`InputLogScenario`] adapts a [`DetInputSim`] and an [`InputLog`] to [`DetScenario`]: `reset`
//! restores the snapshot, `step(t)` replays log tick `start + t` through [`replay::advance`] (the
//! live step path), and `hash_state` feeds the world hash and every system word, named by the sim's
//! systems, so a golden report names the system that moved. Register it in a
//! [`super::DetRegistry`] like any scenario; the engine's own, `input_log_arena`, drives the AI
//! arena from a committed log ([`super::scenarios::input_log_arena_scenario`]).

use crate::golden::{DetRun, DetScenario, StateHasher, TickRate};
use crate::hash::SystemFold;
use crate::replay::{self, DetInputSim, InputLog, ReplayError};
use crate::state::{self, DetReadLimits};

/// A recorded run (start snapshot and input log) replayed as a [`DetScenario`].
pub struct InputLogScenario<S: DetInputSim<N>, const N: usize> {
    name: String,
    start: Vec<u8>,
    log: InputLog<S::Input>,
    rate: TickRate,
    sim: Option<S>,
}

impl<S: DetInputSim<N>, const N: usize> InputLogScenario<S, N> {
    /// A scenario called `name` (the golden's stem; the registry checks the name rules) replaying
    /// `log` from `snapshot`, or from the log's carried snapshot when `None`. Refuses a log of
    /// another sim (`wrong_sim`, `system_count`), no snapshot (`no_snapshot`), and a snapshot that
    /// does not restore as `S` at the log's start tick (its det-state code, `tick_gap`).
    pub fn new(
        name: impl Into<String>,
        snapshot: Option<Vec<u8>>,
        log: InputLog<S::Input>,
    ) -> Result<Self, ReplayError> {
        if (log.sim_tag(), log.sim_version()) != (S::STATE_TAG, S::STATE_VERSION) {
            return Err(ReplayError::WrongSim {
                found: (log.sim_tag(), log.sim_version()),
                expected: (S::STATE_TAG, S::STATE_VERSION),
            });
        }
        if log.system_count() as usize != N {
            return Err(ReplayError::SystemCount {
                found: log.system_count(),
                expected: N as u32,
            });
        }
        let start = match snapshot {
            Some(bytes) => bytes,
            None => log.start_snapshot().ok_or(ReplayError::NoSnapshot)?.to_vec(),
        };
        let (tick, sim) = state::restore::<S>(&start)?;
        if tick != log.start_tick() {
            return Err(ReplayError::TickGap {
                expected: log.start_tick(),
                found: tick,
            });
        }
        Ok(InputLogScenario {
            name: name.into(),
            start,
            log,
            rate: TickRate::DEFAULT,
            sim: Some(sim),
        })
    }

    /// Decode `log_bytes` (bounded by `limits`) and build the scenario ([`InputLogScenario::new`]).
    pub fn from_bytes(
        name: impl Into<String>,
        snapshot: Option<Vec<u8>>,
        log_bytes: &[u8],
        limits: DetReadLimits,
    ) -> Result<Self, ReplayError> {
        Self::new(name, snapshot, InputLog::decode_with(log_bytes, limits)?)
    }

    /// The same scenario with the tick rate its [`DetRun`] reports (default 60 Hz). The rate is
    /// the sim's own business; the golden records it.
    pub fn with_rate(mut self, rate: TickRate) -> Self {
        self.rate = rate;
        self
    }

    /// The log replayed.
    pub fn log(&self) -> &InputLog<S::Input> {
        &self.log
    }

    /// The sim at its current tick (`None` only if a reset failed to restore, which `new` rules
    /// out).
    pub fn sim(&self) -> Option<&S> {
        self.sim.as_ref()
    }
}

impl<S: DetInputSim<N>, const N: usize> DetScenario for InputLogScenario<S, N> {
    fn name(&self) -> &str {
        &self.name
    }

    /// Seed 0 (the inputs come from the log), one tick per log entry, the scenario's rate.
    fn default_run(&self) -> DetRun {
        DetRun::new(0, u32::try_from(self.log.len()).unwrap_or(u32::MAX), self.rate)
    }

    fn reset(&mut self, _run: &DetRun) {
        // `new` restored these bytes once already, so this cannot fail; if it ever did, the hash
        // says so (`restore_failed`) instead of panicking.
        self.sim = state::restore::<S>(&self.start).ok().map(|(_, sim)| sim);
    }

    /// Replay log entry `tick` (log tick `start_tick + tick`). Past the log's end nothing steps.
    fn step(&mut self, tick: u32) {
        let (Some(sim), Some(entry)) = (self.sim.as_mut(), self.log.entries().get(tick as usize)) else {
            return;
        };
        replay::advance(sim, &entry.inputs);
    }

    /// The world hash, then each system's word under the system's name.
    fn hash_state(&self, h: &mut StateHasher) {
        let Some(sim) = &self.sim else {
            h.u64("restore_failed", 1);
            return;
        };
        h.u64("world", <S as DetInputSim<N>>::hash(sim));
        let systems = <S as SystemFold<N>>::SYSTEMS;
        let words = <S as DetInputSim<N>>::system_hashes(sim).words;
        for (name, word) in systems.names().iter().zip(words) {
            h.u64(name, word);
        }
    }

    /// Nudges system `bit % N` through the sim's [`SystemFold::inject`] (the desync injection), so
    /// [`super::compare`] can inject into a replayed run.
    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.sim.as_mut() {
            Some(sim) => crate::hash::SystemFold::inject(sim, bit as usize % N.max(1)),
            None => false,
        }
    }
}
