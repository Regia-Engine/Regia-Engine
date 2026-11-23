// SPDX-License-Identifier: MIT-0
//! Arena configuration and the seeded match RNG.
//!
//! Every source of nondeterminism in the match flows through [`MatchRng`] — bot target choice,
//! steering jitter, weapon spread, respawn point selection. With a fixed `--seed` (and a fixed
//! timestep), the whole bot match is a pure function of the seed: two runs replay to the same
//! final score (the determinism acceptance, `samples/arena/tests/determinism.rs`).

use regia_app::prelude::*;
use regia_core::math::Rng;
use regia_framework::rules::phases::{PhaseSettings, TimeUp};

/// The seed used when no `--seed` flag is given.
pub const DEFAULT_SEED: u64 = 0x5EED_0000_0000_0017;

/// Match parameters (`docs/specs/14` M20 `samples/arena`).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ArenaConfig {
    /// Kills needed to win the match.
    pub score_limit: u32,
    /// Warm-up duration in seconds (bots idle, no scoring).
    pub warmup_secs: f32,
    /// Hard match timer in seconds; whoever leads when it hits zero wins.
    pub match_secs: f32,
    /// Number of AI bots in the match (plus the one local player).
    pub bot_count: usize,
    /// Whether a local player participates (off for a pure bot-vs-bot determinism match).
    pub local_player: bool,
    /// When `true`, bots navigate on a navmesh and are driven by a behaviour tree instead of
    /// the plain steering path (specs/22 § T2.2). Off by default so determinism replays stay on
    /// the steering path.
    pub use_navmesh: bool,
    /// What drives the bots when `use_navmesh` is off: the state tree with abilities, the weapon
    /// chooser and resupply stations (default, ADR-2134), or the plain steering path.
    pub brain: BotBrainKind,
}

/// The bot brain of a match without a navmesh (`ArenaConfig::brain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BotBrainKind {
    /// `assets/ai/bot.statetree.ron` (`crate::brain`): fight, restock at a station, repeat.
    #[default]
    StateTree,
    /// The steering loop in `crate::bots` (no abilities, no stations).
    Steering,
}

impl ArenaConfig {
    /// The match core's phase settings for this config at `tick_hz` fixed steps per second
    /// (ADR-8485): the warm-up and the match timer become tick counts once, here. A zero warm-up
    /// is no warm-up; the timer always runs, and its leader wins (a tie ends with no winner).
    pub fn phase_settings(&self, tick_hz: f32) -> PhaseSettings {
        // Rounded: `1 / step` of a 60 Hz fixed step is a hair under 60 in f32.
        let ticks = |secs: f32| (secs.max(0.0) * tick_hz).round() as u64;
        let warmup = ticks(self.warmup_secs);
        PhaseSettings {
            warmup_ticks: (warmup > 0).then_some(warmup),
            time_limit_ticks: Some(ticks(self.match_secs).max(1)),
            on_time_up: TimeUp::End,
            ..PhaseSettings::default()
        }
    }
}

impl Default for ArenaConfig {
    fn default() -> Self {
        Self {
            score_limit: 10,
            warmup_secs: 3.0,
            match_secs: 180.0,
            bot_count: 7,
            local_player: true,
            use_navmesh: false,
            brain: BotBrainKind::StateTree,
        }
    }
}

/// Which bot brain this match runs and what it set up (a component on the `"match"` entity, next to
/// `ArenaState`, so `play.expect` can read it).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct ArenaBrainStatus {
    /// `state_tree`, `steering` or `navmesh`.
    pub brain: String,
    /// Bots that carry a compiled state tree.
    pub trees: u32,
    /// Resupply stations (smart objects) in the arena.
    pub stations: u32,
    /// Errors from loading the embedded data (`LoadoutDiagnostics`) or compiling the bot tree; a
    /// state-tree match that fails to set up falls back to steering.
    pub errors: Vec<String>,
}

/// The match's single deterministic random source.
///
/// Seeded from `ArenaConfig` (via the `--seed` flag on the launcher) at startup; replaced with a
/// fresh generator between runs in determinism tests. All combat and bot logic draws from it.
#[derive(Resource, Debug)]
pub struct MatchRng {
    /// The generator.
    pub rng: Rng,
}

impl MatchRng {
    /// A generator seeded from `seed`.
    pub fn new(seed: u64) -> Self {
        Self { rng: Rng::new(seed) }
    }
}

impl Default for MatchRng {
    fn default() -> Self {
        Self::new(DEFAULT_SEED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_rng_is_reproducible() {
        let mut a = MatchRng::new(42);
        let mut b = MatchRng::new(42);
        for _ in 0..100 {
            assert_eq!(a.rng.f32(), b.rng.f32(), "same seed → same stream");
        }
        let mut c = MatchRng::new(43);
        assert_ne!(a.rng.f32(), c.rng.f32(), "different seed → different stream");
    }

    #[test]
    fn phase_settings_are_tick_counts_of_the_config() {
        let c = ArenaConfig::default();
        let phases = c.phase_settings(60.0);
        assert_eq!(phases.warmup_ticks, Some(180));
        assert_eq!(phases.time_limit_ticks, Some(10_800));
        let no_warmup = ArenaConfig {
            warmup_secs: 0.0,
            ..ArenaConfig::default()
        };
        assert_eq!(no_warmup.phase_settings(60.0).warmup_ticks, None);
        assert!(phases.validate().is_ok());
        let step = 1.0f32 / 60.0;
        assert_eq!(c.phase_settings(1.0 / step).time_limit_ticks, Some(10_800));
    }

    #[test]
    fn config_defaults_are_sensible() {
        let c = ArenaConfig::default();
        assert!(c.score_limit > 0);
        assert!(c.warmup_secs >= 0.0);
        assert!(c.match_secs > 0.0);
        assert!(c.bot_count > 0);
    }
}
