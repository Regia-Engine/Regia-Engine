// SPDX-License-Identifier: MPL-2.0
//! Cloth states, blend ramps and distance LOD (spec 55 § 2.9).
//!
//! The state machine lives in the solver ([`ClothSolver::state`]); this module holds the state
//! enum, the step counts, the pure distance-LOD rule with 10 % hysteresis and the particle
//! budget that pushes the farthest solvers down one level.
//!
//! [`ClothSolver::state`]: super::ClothSolver::state

use regia_ecs::spawn_order::SpawnKey;

/// Fixed steps a cloth spends in [`ClothState::Settling`] (at double damping).
pub const SETTLE_STEPS: u32 = 8;
/// Fixed steps the blend takes to ramp in after settling starts.
pub const BLEND_IN_STEPS: u32 = 30;
/// Fixed steps the blend takes to ramp out in [`ClothState::Fading`].
pub const BLEND_OUT_STEPS: u32 = 20;
/// Paused fixed steps after which resuming re-enters [`ClothState::Settling`].
pub const RESUME_RESET_STEPS: u32 = 600;
/// Hysteresis of the LOD distances (10 %).
pub const LOD_HYSTERESIS: f32 = 0.1;
/// Default particle budget per fixed step on desktop.
pub const BUDGET_DESKTOP: usize = 8_192;
/// Default particle budget per fixed step on Android.
pub const BUDGET_ANDROID: usize = 4_096;

/// Lifecycle state of a cloth (spec 55 § 2.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClothState {
    /// No simulation; the output is the pose.
    Inactive,
    /// Particles start at the pose with zero velocity; damping is doubled; the blend ramps in.
    Settling,
    /// Normal simulation.
    Running,
    /// LOD off requested: the blend ramps to 0 while simulating, then [`ClothState::Inactive`].
    Fading,
    /// Not visible or paused by an agent: no stepping, state kept.
    Paused,
}

impl ClothState {
    /// Stable lowercase name for the agent protocol (`inactive`, `settling`, `running`,
    /// `fading`, `paused`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Settling => "settling",
            Self::Running => "running",
            Self::Fading => "fading",
            Self::Paused => "paused",
        }
    }

    pub(crate) fn tag(&self) -> u8 {
        match self {
            Self::Inactive => 0,
            Self::Settling => 1,
            Self::Running => 2,
            Self::Fading => 3,
            Self::Paused => 4,
        }
    }
}

/// A distance LOD level: 0 = authored, 1 and 2 cheaper, [`LOD_OFF`] = fade to pose.
pub type LodLevel = u8;

/// The "off" LOD level.
pub const LOD_OFF: LodLevel = 3;

/// The LOD level for a viewer at `distance` from a cloth currently at `current`, with
/// thresholds `[d1, d2, d_off]` (ascending). A level is left outward only past `d * 1.1` and
/// inward only inside `d * 0.9`.
#[must_use]
pub fn lod_level(distance: f32, current: LodLevel, distances: [f32; 3]) -> LodLevel {
    let current = current.min(LOD_OFF);
    if !distance.is_finite() {
        return current;
    }
    // The level the distance gives without hysteresis.
    let plain = distances.iter().filter(|&&d| distance >= d).count() as LodLevel;
    match plain.cmp(&current) {
        std::cmp::Ordering::Equal => current,
        std::cmp::Ordering::Greater => {
            // Moving outward: every crossed threshold must be passed by 10 %.
            let mut level = current;
            while level < plain && distance >= distances[level as usize] * (1.0 + LOD_HYSTERESIS) {
                level += 1;
            }
            level
        }
        std::cmp::Ordering::Less => {
            let mut level = current;
            while level > plain && distance < distances[level as usize - 1] * (1.0 - LOD_HYSTERESIS) {
                level -= 1;
            }
            level
        }
    }
}

/// One solver's entry for [`apply_budget`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetEntry {
    /// Viewer distance in meters.
    pub distance: f32,
    /// Stable key (the instance's spawn-arrival key, `regia_ecs::spawn_order::SpawnKey`) that
    /// breaks distance ties.
    pub key: SpawnKey,
    /// Particles the solver simulates at LOD 0.
    pub particles: usize,
    /// The level the distance rule chose; raised by [`apply_budget`].
    pub level: LodLevel,
}

/// Particles a solver of `particles` particles costs at `level` (levels 1 and 2 halve and
/// quarter the substeps, so they count as half and a quarter; off counts nothing).
#[must_use]
pub fn budget_cost(particles: usize, level: LodLevel) -> usize {
    match level {
        0 => particles,
        1 => particles.div_ceil(2),
        2 => particles.div_ceil(4),
        _ => 0,
    }
}

/// Moves the farthest solvers (by distance, then key) down one level at a time until the
/// summed cost fits `max_particles_per_step`. Returns the final cost.
pub fn apply_budget(entries: &mut [BudgetEntry], max_particles_per_step: usize) -> usize {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| {
        entries[b]
            .distance
            .total_cmp(&entries[a].distance)
            .then(entries[b].key.cmp(&entries[a].key))
    });
    let cost = |e: &[BudgetEntry]| e.iter().map(|x| budget_cost(x.particles, x.level)).sum::<usize>();
    let mut total = cost(entries);
    while total > max_particles_per_step {
        let Some(&victim) = order.iter().find(|&&i| entries[i].level < LOD_OFF) else {
            break;
        };
        entries[victim].level += 1;
        total = cost(entries);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: [f32; 3] = [10.0, 20.0, 40.0];

    #[test]
    fn levels_change_only_past_hysteresis() {
        assert_eq!(lod_level(5.0, 0, D), 0);
        assert_eq!(lod_level(10.5, 0, D), 0, "inside the 10 % band");
        assert_eq!(lod_level(11.1, 0, D), 1);
        assert_eq!(lod_level(9.5, 1, D), 1, "inside the band inward");
        assert_eq!(lod_level(8.9, 1, D), 0);
        assert_eq!(lod_level(50.0, 0, D), LOD_OFF);
        assert_eq!(lod_level(37.0, LOD_OFF, D), LOD_OFF);
        assert_eq!(lod_level(35.0, LOD_OFF, D), 2);
        assert_eq!(lod_level(f32::NAN, 2, D), 2);
    }

    #[test]
    fn budget_pushes_the_farthest_down_first() {
        let mut e = [
            BudgetEntry {
                distance: 5.0,
                key: (1, 0, 0),
                particles: 4000,
                level: 0,
            },
            BudgetEntry {
                distance: 30.0,
                key: (2, 0, 0),
                particles: 4000,
                level: 0,
            },
            BudgetEntry {
                distance: 30.0,
                key: (3, 0, 0),
                particles: 4000,
                level: 0,
            },
        ];
        let total = apply_budget(&mut e, 8192);
        assert!(total <= 8192, "{total}");
        assert_eq!(e[0].level, 0, "the nearest keeps full detail");
        assert!(e[2].level >= e[1].level, "ties: the higher key goes first");
        let mut tiny = e;
        assert_eq!(apply_budget(&mut tiny, 0), 0);
        assert!(tiny.iter().all(|x| x.level == LOD_OFF));
    }
}
