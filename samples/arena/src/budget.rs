// SPDX-License-Identifier: MIT-0
//! Budget probe load (`docs/specs/14` M20): 200 dynamic crates and a 24,000-particle burst.
//!
//! Bots come from the match (`ArenaConfig::bot_count`). The particle burst is one
//! [`regia_vfx::VfxSystemDef`] with no distance LOD, so [`VfxScene::estimated_live`] reaches
//! the acceptance count on the CPU sim (the GPU pool is a separate cap). Lavapipe frame times
//! from `--bench` are not representative; the lead runs that command on the real GPU.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::format::{
    EmitterDef, InitModule, RangeDef, RenderModule, ShapeDef, SpawnMode, SpawnModule, SystemModule,
    UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VfxSystemDef,
};
use regia_vfx::{VfxScene, VfxSettings};

use crate::props::spawn_many_props;

/// Bots the probe must have alive.
pub const BUDGET_BOTS: usize = 8;
/// Dynamic crates added on top of the arena's built-in props.
pub const BUDGET_BODIES: usize = 200;
/// Live particles the probe must reach (`estimated_live`).
pub const BUDGET_PARTICLES: u32 = 20_000;
/// One-shot burst. Above [`BUDGET_PARTICLES`] so a single spawn clears the bar.
const BURST: u32 = 24_000;

/// Counts the probe checks against the live world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetCounts {
    /// Pawns with `Pawn::is_bot`.
    pub bots: usize,
    /// Rigid bodies in the physics world (static and dynamic).
    pub bodies: usize,
    /// CPU estimate of live particles.
    pub particles: u32,
}

impl BudgetCounts {
    /// Whether this snapshot meets the M20 budget probe.
    pub fn meets_budget(self) -> bool {
        self.bots >= BUDGET_BOTS && self.bodies >= BUDGET_BODIES && self.particles >= BUDGET_PARTICLES
    }
}

/// Reads bots, rigid bodies and live particles.
pub fn budget_counts(world: &mut World) -> BudgetCounts {
    let bots = {
        let mut pawns = world.query::<&crate::Pawn>();
        pawns.iter(world).filter(|pawn| pawn.is_bot).count()
    };
    let bodies = world
        .get_resource::<regia_physics::PhysicsWorld>()
        .map(|physics| physics.bodies.len())
        .unwrap_or(0);
    let particles = world
        .get_resource::<VfxScene>()
        .map(|scene| scene.estimated_live())
        .unwrap_or(0);
    BudgetCounts {
        bots,
        bodies,
        particles,
    }
}

/// Spawns [`BUDGET_BODIES`] dynamic crates and one particle burst of 24_000.
///
/// Raises the VFX profile capacity when it is below the burst. Safe to call once the
/// [`VfxScene`] resource exists (the arena plugin inserts it).
pub fn apply_budget_load(world: &mut World) {
    if let Some(mut settings) = world.get_resource_mut::<VfxSettings>() {
        settings.profile.particle_capacity = settings.profile.particle_capacity.max(BURST);
    }
    spawn_budget_burst(world);
    spawn_many_props(world, BUDGET_BODIES);
}

fn budget_fill_def() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "budget_fill".to_string(),
        system: SystemModule {
            bounds: None,
            budget: BURST,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "fill".to_string(),
            delay: 0.0,
            duration: Some(0.1),
            spawn: SpawnModule {
                mode: SpawnMode::Burst(BURST),
                lifetime: RangeDef { min: 30.0, max: 30.0 },
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::default(),
                velocity: VelocityDef::default(),
                size: RangeDef { min: 0.04, max: 0.08 },
                rotation: RangeDef::default(),
                spin: RangeDef::default(),
                color: Vec::new(),
            },
            update: UpdateModule::default(),
            render: RenderModule::default(),
        }],
    }
}

fn spawn_budget_burst(world: &mut World) {
    let Some(mut scene) = world.get_resource_mut::<VfxScene>() else {
        tracing::warn!(target: "arena", "budget probe: no VfxScene; particles will be zero");
        return;
    };
    if scene.library().index("budget_fill").is_none()
        && let Err(err) = scene.library_mut().add(budget_fill_def())
    {
        tracing::warn!(target: "arena", "budget probe: effect rejected: {err}");
        return;
    }
    let _ = scene.spawn("budget_fill", Vec3::new(0.0, 2.0, 0.0), Vec3::Y, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_vfx::sim::LodInput;

    #[test]
    fn one_burst_reaches_the_particle_budget() {
        let mut scene = VfxScene::new(regia_vfx::scene::VfxLibrary::new());
        scene
            .library_mut()
            .add(budget_fill_def())
            .expect("budget effect validates");
        assert!(scene.spawn("budget_fill", Vec3::Y, Vec3::Y, 1).is_some());
        scene.update(
            1.0 / 60.0,
            &LodInput {
                camera_pos: Vec3::ZERO,
            },
            BURST,
        );
        assert!(
            scene.estimated_live() >= BUDGET_PARTICLES,
            "live {}",
            scene.estimated_live()
        );
    }
}
