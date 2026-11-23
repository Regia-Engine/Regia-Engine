// SPDX-License-Identifier: MPL-2.0
//! Light-emitting particles with a per-profile budget.
//!
//! Emitters with a `light` module produce one point light each, positioned at the instance origin
//! and scaled by the burst's fire envelope and the instance intensity. The gather step walks
//! active instances and publishes [`VfxFrameLights`], strongest-first and capped by the `budget`
//! argument of `VfxScene::gather_lights` (the profile's light budget), so a spike degrades
//! gracefully instead of flickering.
//!
//! The offscreen harness shades the strongest light on the ground for the flash window.
//! A host that does not call [`crate::offscreen::VfxHarness::set_point_light`] draws none.

use regia_core::math::Vec3;
use regia_ecs::prelude::*;

/// Profile cap on live particle lights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfxLightBudget(pub u32);

impl Default for VfxLightBudget {
    fn default() -> Self {
        Self(16)
    }
}

/// One light for the current frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParticleLight {
    /// World position.
    pub position: Vec3,
    /// Linear RGB colour × intensity.
    pub color: [f32; 3],
    /// Radius of influence (metres).
    pub radius: f32,
}

/// The lights published for this frame, capped by the budget.
#[derive(Debug, Clone, Default, Resource)]
pub struct VfxFrameLights {
    /// Active lights, strongest first.
    pub lights: Vec<ParticleLight>,
}

impl VfxFrameLights {
    /// Gathers lights from candidates, keeping the strongest `budget` (intensity × πr² as the
    /// ranking heuristic).
    pub fn gather(&mut self, mut candidates: Vec<ParticleLight>, budget: u32) {
        candidates.sort_by(|a, b| {
            let ea = light_energy(a);
            let eb = light_energy(b);
            eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(budget as usize);
        self.lights = candidates;
    }

    /// Number of lights dropped by the budget this frame (stat).
    pub fn overflow(&self, total: usize) -> usize {
        total.saturating_sub(self.lights.len())
    }
}

fn light_energy(l: &ParticleLight) -> f32 {
    let c = l.color;
    let peak = c[0].max(c[1]).max(c[2]);
    peak * l.radius * l.radius
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_keeps_strongest() {
        let mut frame = VfxFrameLights::default();
        let lights = vec![
            ParticleLight {
                position: Vec3::ZERO,
                color: [0.1, 0.1, 0.1],
                radius: 1.0,
            },
            ParticleLight {
                position: Vec3::ONE,
                color: [10.0, 8.0, 4.0],
                radius: 4.0,
            },
            ParticleLight {
                position: Vec3::new(2.0, 0.0, 0.0),
                color: [2.0, 2.0, 2.0],
                radius: 2.0,
            },
        ];
        frame.gather(lights, 2);
        assert_eq!(frame.lights.len(), 2);
        assert_eq!(frame.lights[0].position, Vec3::ONE, "bright big light first");
        assert_eq!(frame.overflow(3), 1);
    }

    #[test]
    fn zero_budget_empties() {
        let mut frame = VfxFrameLights::default();
        frame.gather(
            vec![ParticleLight {
                position: Vec3::ZERO,
                color: [1.0; 3],
                radius: 1.0,
            }],
            0,
        );
        assert!(frame.lights.is_empty());
    }
}
