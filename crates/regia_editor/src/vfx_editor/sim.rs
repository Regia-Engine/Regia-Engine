// SPDX-License-Identifier: MPL-2.0
//! CPU measurements of the open effect: a fixed-seed simulation fingerprint (the A/B check) and the
//! per-emitter cost rows.
//!
//! Both run `regia_vfx`'s own CPU paths: [`regia_vfx::probe::probe_steps`] (the CPU reference of the
//! GPU update, events and kills included) for live particles and positions, and
//! [`regia_vfx::sim::VfxInstance::schedule`] (the spawn scheduler the scene runs) for spawn counts.
//! Nothing here touches the GPU, so the numbers are the same on every machine.

use regia_core::math::Vec3;
use regia_vfx::format::{SpawnMode, VfxSystemDef};
use regia_vfx::probe::{ProbeSettings, probe_steps};
use regia_vfx::sim::{LodInput, SpawnJob, VfxInstance};

/// The fixed step every editor measurement uses (seconds).
pub const STEP: f32 = 1.0 / 60.0;

/// Where the editor fires the effect: 0.5 m above the preview ground, local +Z up.
pub const ORIGIN: Vec3 = Vec3::new(0.0, 0.5, 0.0);

/// The preview's forward axis (local +Z): straight up, like an effect played on the ground.
pub const FORWARD: Vec3 = Vec3::Y;

/// Most steps one measurement may run (10 minutes at 60 Hz).
pub const MAX_STEPS: u32 = 36_000;

/// A fixed-seed simulation fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimHash {
    /// Steps run.
    pub steps: u32,
    /// Live particles after the last step.
    pub particles: u32,
    /// FNV-1a over every live particle's emitter index, position, velocity, age and lifetime bits.
    pub hash: u64,
}

/// FNV-1a 64 over 32-bit words (Fowler, Noll, Vo; public domain reference parameters).
fn fnv(words: impl IntoIterator<Item = u32>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn settings(seed: u64) -> ProbeSettings {
    ProbeSettings {
        dt: STEP,
        seed,
        ..ProbeSettings::default()
    }
}

/// Runs `def` for `steps` fixed steps with `seed` and fingerprints the live particles.
#[must_use]
pub fn sim_hash(def: &VfxSystemDef, seed: u64, steps: u32) -> SimHash {
    let _span = tracing::info_span!("vfx_editor.sim_hash", steps).entered();
    let steps = steps.min(MAX_STEPS);
    let (states, stats) = probe_steps(def, ORIGIN, FORWARD, settings(seed), steps);
    let mut words = Vec::with_capacity(states.len() * 9 + 2);
    for s in &states {
        words.push(s.emitter as u32);
        words.extend(
            s.position
                .to_array()
                .iter()
                .chain(&s.velocity.to_array())
                .map(|v| v.to_bits()),
        );
        words.extend([s.age.to_bits(), s.life.to_bits()]);
    }
    words.extend([stats.fired, stats.children_spawned]);
    SimHash {
        steps,
        particles: states.len() as u32,
        hash: fnv(words),
    }
}

/// One emitter's cost row.
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterCost {
    /// Emitter name.
    pub name: String,
    /// Live particles at the measured time (CPU reference simulation).
    pub alive: u32,
    /// Particles spawned per second over the last second before the measured time (`None` for an
    /// event-driven emitter, whose spawns come from its parents' particles).
    pub spawned_per_sec: Option<f32>,
    /// The draw list the emitter feeds (`additive`, `alpha`, `mesh`, `ribbon`, `haze`).
    pub draw_list: &'static str,
    /// Draw calls of that list this frame (each list is one indirect draw shared by its emitters;
    /// 0 when no emitter of the list has a live particle).
    pub draw_calls: u32,
    /// The emitter's share of the measured GPU VFX time in ms (simulate + draw passes, split by
    /// live particles), when the device has timestamp queries and a preview frame was timed.
    pub gpu_ms: Option<f32>,
}

/// The per-emitter cost of `def` at `time` seconds with `seed`, from the CPU paths. `gpu_total_ms`
/// is the timed GPU VFX cost of the frame (from the preview), split by live particles when given.
#[must_use]
pub fn emitter_costs(
    def: &VfxSystemDef,
    seed: u64,
    time: f32,
    gpu_total_ms: Option<f32>,
) -> Vec<EmitterCost> {
    let _span = tracing::info_span!("vfx_editor.emitter_costs").entered();
    let steps = steps_for(time);
    let (states, _) = probe_steps(def, ORIGIN, FORWARD, settings(seed), steps);
    let mut alive = vec![0u32; def.emitters.len()];
    for s in &states {
        if let Some(a) = alive.get_mut(s.emitter) {
            *a += 1;
        }
    }
    // Spawns over the last second (60 steps) of the run, through the scene's scheduler.
    let window = steps.min(60);
    let mut spawned = vec![0u32; def.emitters.len()];
    let mut instance = VfxInstance::new(0, seed, ORIGIN, FORWARD);
    let lod = LodInput {
        camera_pos: ORIGIN + Vec3::new(0.0, 2.0, 8.0),
    };
    let mut jobs: Vec<SpawnJob> = Vec::new();
    for step in 0..steps {
        instance.schedule(def, STEP, &lod, u32::MAX);
        for (i, count) in spawned.iter_mut().enumerate() {
            jobs.clear();
            instance.drain_emitter_jobs(i, &mut jobs);
            if step >= steps - window {
                *count += jobs.len() as u32;
            }
        }
    }
    let seconds = window as f32 * STEP;
    let total_alive: u32 = alive.iter().sum();
    let lists: Vec<&'static str> = def.emitters.iter().map(super::modules::draw_list).collect();
    def.emitters
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let list = lists[i];
            let list_live = lists.iter().zip(&alive).any(|(l, a)| *l == list && *a > 0);
            EmitterCost {
                name: e.name.clone(),
                alive: alive[i],
                spawned_per_sec: (e.spawn.mode != SpawnMode::Event && seconds > 0.0)
                    .then(|| spawned[i] as f32 / seconds),
                draw_list: list,
                draw_calls: u32::from(list_live),
                gpu_ms: gpu_total_ms.map(|ms| {
                    if total_alive == 0 {
                        0.0
                    } else {
                        ms * alive[i] as f32 / total_alive as f32
                    }
                }),
            }
        })
        .collect()
}

/// Steps of [`STEP`] that reach `time` seconds (clamped to `0..=MAX_STEPS`).
#[must_use]
pub fn steps_for(time: f32) -> u32 {
    if !time.is_finite() || time <= 0.0 {
        return 0;
    }
    ((time / STEP).round() as u32).min(MAX_STEPS)
}
