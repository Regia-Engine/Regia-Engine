// Written by the spike host before each rebuild (variant A). Do not edit by hand.
use spike_regia::hot::GameRegistrar;
use spike_regia::prelude::*;

/// Generation number of this build.
pub const GEN: u32 = 1;
/// Name of this variant's movement system.
pub const SYSTEM_NAME: &str = "move_a";

/// Variant A: `legacy` exists, `heading` does not.
#[derive(Component, Reflect, Debug)]
#[reflect(Component, Default)]
pub struct Mover {
    /// Preserved across all generations.
    pub speed: f32,
    /// Removed in variant B.
    pub legacy: u32,
    /// Preserved; incremented by the movement system.
    pub ticks: u64,
}

impl Default for Mover {
    fn default() -> Self {
        Self {
            speed: 1.0,
            legacy: 7,
            ticks: 0,
        }
    }
}

/// Variant A movement system.
pub fn move_a(mut q: Query<&mut Mover>, mut score: ResMut<crate::Score>) {
    for mut m in &mut q {
        m.ticks += 1;
    }
    score.gen_seen = GEN;
}

/// Registers this variant's types and systems.
pub fn register(reg: &mut GameRegistrar<'_>) {
    reg.component::<Mover>();
    reg.add_systems(Update, move_a);
}
