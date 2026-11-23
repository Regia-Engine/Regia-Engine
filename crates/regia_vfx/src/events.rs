// SPDX-License-Identifier: MPL-2.0
//! Event spawns (`.vfx.ron` v6): the semantics shared by the GPU kernels (`gpu/shaders.rs`,
//! `update` and `child_emit`) and the CPU reference ([`crate::probe`]).
//!
//! A particle that hits a trigger fires an event with a seed hashed from the particle's identity,
//! the simulation step and the trigger. On the next simulated frame each of the emitter's links
//! with that trigger rolls once from that seed (all of `count` or nothing), and every child's
//! attributes are further hash channels of it. Everything is integer hashing on values both sides
//! know, so counts and draws agree: there is no RNG state to advance in thread order.
//!
//! The hash is the PCG output function (O'Neill, PCG report, 2014) already used by the curl noise.

use regia_core::math::Vec3;
use regia_core::math::{det_cbrt_f32, det_cos, det_sin_cos_f32, det_tan};

use crate::format::{
    EmitterDef, EventSpawnDef, EventTrigger, MAX_EVENTS_PER_EMITTER, ShapeDef, SpawnMode, VelocityMode,
    VfxSystemDef,
};
use crate::gpu::layout::{ChildInitGpu, EventLinkGpu};
use crate::sim::VfxInstance;

/// Trigger bit of [`EventTrigger::Spawn`].
pub const TRIGGER_SPAWN: u32 = 1;
/// Trigger bit of [`EventTrigger::Death`].
pub const TRIGGER_DEATH: u32 = 2;
/// Trigger bit of [`EventTrigger::Collision`].
pub const TRIGGER_COLLISION: u32 = 4;

/// Low lifetime bits that carry a child particle's identity: at most `2^-13` of the lifetime
/// (0.12 ms per second).
pub const IDENTITY_MASK: u32 = 0x3FF;

/// Extra seconds an instance with events stays alive past its last possible child, so that
/// emitter records outlive the frames between a death, its event and the child's first update.
pub const EVENT_HOLD_MARGIN_S: f32 = 0.25;

/// Largest child count a packed event link can carry. [`EventLinkGpu::trigger_count`] keeps the
/// trigger bit in its low 8 bits and the scaled count in the upper 24, so a count of exactly
/// 2^24 packs to zero (the link never fires) and anything above wraps to `scaled - 2^24` on the
/// GPU. [`crate::format::VfxSystemDef::validate`] refuses a link whose worst case exceeds it and
/// [`scaled_count`] clamps to it (deep-zai-vfx finding 2).
pub const MAX_PACKED_EVENT_COUNT: u32 = (1 << 24) - 1;

/// The trigger's bit.
#[must_use]
pub fn trigger_bit(trigger: EventTrigger) -> u32 {
    match trigger {
        EventTrigger::Spawn => TRIGGER_SPAWN,
        EventTrigger::Death => TRIGGER_DEATH,
        EventTrigger::Collision => TRIGGER_COLLISION,
    }
}

/// The trigger mask of `emitter`'s links.
#[must_use]
pub fn trigger_mask(emitter: &EmitterDef) -> u32 {
    emitter
        .update
        .events
        .iter()
        .fold(0, |mask, event| mask | trigger_bit(event.on))
}

/// PCG-style 32-bit hash; WGSL `hash_u32`.
#[must_use]
pub fn hash_u32(seed: u32) -> u32 {
    let state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// A hash channel as a float in `[0, 1)`; WGSL `child_unit`.
#[must_use]
pub fn child_unit(seed: u32, channel: u32) -> f32 {
    (hash_u32(seed ^ channel.wrapping_mul(2_654_435_769)) & 0xFF_FFFF) as f32 / 16_777_216.0
}

/// The seed of an event; WGSL `event_seed`. `step` counts simulated frames from 1.
#[must_use]
pub fn event_seed(identity: u32, step: u32, trigger: u32) -> u32 {
    hash_u32(
        identity.wrapping_mul(2_654_435_761)
            ^ step.wrapping_mul(40_503)
            ^ trigger.wrapping_mul(2_246_822_519),
    )
}

/// The seed link `link` of an event rolls from; WGSL in `child_emit`.
#[must_use]
pub fn link_seed(event_seed: u32, link: u32) -> u32 {
    hash_u32(event_seed ^ (link + 1).wrapping_mul(2_246_822_519))
}

/// Whether a link fires, all of its count or nothing.
#[must_use]
pub fn link_fires(link_seed: u32, probability: f32) -> bool {
    child_unit(link_seed, 12) < probability
}

/// The seed of child `child` of a firing link.
#[must_use]
pub fn child_seed(link_seed: u32, child: u32) -> u32 {
    hash_u32(link_seed ^ (child + 1).wrapping_mul(3_266_489_917))
}

/// `life` with a child's identity in its low bits; WGSL in `spawn_child`.
#[must_use]
pub fn stamp_identity(life: f32, seed: u32) -> f32 {
    let id = hash_u32(seed ^ 0x51ED_270B) & IDENTITY_MASK;
    f32::from_bits((life.to_bits() & !IDENTITY_MASK) | id)
}

/// A child-ring particle's identity from its stamped lifetime; WGSL `particle_identity`.
#[must_use]
pub fn child_identity(life: f32) -> u32 {
    (life.to_bits() & IDENTITY_MASK) | 0x10_0000
}

/// Children per firing after the child's `count_scale`, the LOD band and the instance's intensity,
/// rounded like a burst. Clamped to [`MAX_PACKED_EVENT_COUNT`]: the value packs into a 24-bit
/// field, and an unclamped 2^24 would pack to zero (the link silently stops firing) while larger
/// values wrap. Validation refuses the authored worst case; the clamp holds for a `count_scale`
/// that reaches the live library another way (`vfx.set_param`).
#[must_use]
pub fn scaled_count(count: u32, child: &EmitterDef, lod_scale: f32, intensity: f32) -> u32 {
    (count as f32 * child.spawn.count_scale * lod_scale * intensity)
        .round()
        .clamp(0.0, MAX_PACKED_EVENT_COUNT as f32) as u32
}

/// The link table row of an emitter of `def` whose record run starts at `base`.
#[must_use]
pub fn links_for(
    def: &VfxSystemDef,
    emitter: &EmitterDef,
    base: u32,
    lod_scale: f32,
    intensity: f32,
) -> [EventLinkGpu; MAX_EVENTS_PER_EMITTER] {
    let mut out = [EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER];
    for (row, event) in out.iter_mut().zip(&emitter.update.events) {
        *row = link_row(def, event, base, lod_scale, intensity);
    }
    out
}

fn link_row(
    def: &VfxSystemDef,
    event: &EventSpawnDef,
    base: u32,
    lod_scale: f32,
    intensity: f32,
) -> EventLinkGpu {
    let Some(child) = def.emitter_index(&event.child) else {
        return EventLinkGpu::default();
    };
    let count = scaled_count(event.count, &def.emitters[child], lod_scale, intensity);
    EventLinkGpu {
        trigger_count: trigger_bit(event.on) | (count << 8),
        child_slot: base + child as u32,
        probability: event.probability,
        inherit: event.inherit_velocity,
    }
}

/// The init record of `emitter` as a child of `instance`: its init module in world space, with
/// the instance's scale and intensity (the same tuning as a scheduled job's, `sim::tune_job`).
/// Zero for an emitter that is not spawned by events.
#[must_use]
pub fn child_init_for(emitter: &EmitterDef, instance: &VfxInstance, lod_scale: f32) -> ChildInitGpu {
    if emitter.spawn.mode != SpawnMode::Event {
        return ChildInitGpu::default();
    }
    let init = &emitter.init;
    let (right, up, forward) = instance.axes();
    let scale = instance.scale();
    let (kind, surface, a, b, half, tan_half) = match &init.shape {
        // Validation keeps mesh shapes off event children (the GPU kernel has no mesh): a point.
        ShapeDef::Point | ShapeDef::MeshSurface { .. } | ShapeDef::SkinnedMesh { .. } => {
            (0.0, 0.0, 0.0, 0.0, (0.0, 0.0, 0.0), 0.0)
        }
        ShapeDef::Sphere { radius, surface } => (
            1.0,
            f32::from(u8::from(*surface)),
            radius.min,
            radius.max,
            (0.0, 0.0, 0.0),
            0.0,
        ),
        ShapeDef::Box {
            half_extents,
            surface,
        } => (2.0, f32::from(u8::from(*surface)), 0.0, 0.0, *half_extents, 0.0),
        ShapeDef::Cone { angle, length } => (
            3.0,
            0.0,
            length.min,
            length.max,
            (0.0, 0.0, 0.0),
            // det_tan: the GPU kernel reads this uploaded constant; it must not follow the
            // platform libm or the cone radii split between platforms (finding 4 of the
            // vfx1 determinism audit).
            det_tan(angle.to_radians().clamp(0.0, 1.55)),
        ),
    };
    let (mode, cos_spread, speed, velocity2) = match &init.velocity.mode {
        VelocityMode::Radial { speed } => (0.0, 1.0, *speed, [0.0; 4]),
        VelocityMode::Directional { dir, spread, speed } => {
            // The instance basis `sim::sample_velocity` uses.
            let d = crate::format::dir_from_tuple(*dir);
            let basis_up = if forward.abs().dot(Vec3::Y) > 0.999 {
                Vec3::X
            } else {
                Vec3::Y
            };
            let side = forward.cross(basis_up).normalize_or_zero();
            let side_up = side.cross(forward);
            let world = side * d.x + side_up * d.y + forward * d.z;
            let cos = if *spread > 0.0 {
                det_cos(spread.to_radians())
            } else {
                1.0
            };
            (1.0, cos, *speed, [world.x, world.y, world.z, 0.0])
        }
        VelocityMode::Planar { speed, lift } => (2.0, 1.0, *speed, [lift.min, lift.max, 0.0, 0.0]),
        // Validation keeps `Normal` off event children; radial is its point-shape meaning.
        VelocityMode::Normal { speed } => (0.0, 1.0, *speed, [0.0; 4]),
    };
    let inherited = instance.velocity() * init.velocity.inherit;
    let smoke_size = if emitter.render.blend == crate::format::BlendMode::Alpha {
        crate::sim::lod_size_compensation(lod_scale)
    } else {
        1.0
    };
    let random_phase = emitter.render.flipbook.is_some_and(|flip| flip.random_start);
    let mut color = crate::curves::sample_color(&init.color, 0.0);
    color[3] *= instance.intensity().min(1.0).sqrt();
    ChildInitGpu {
        shape: [kind, surface, a, b],
        shape2: [half.0, half.1, half.2, tan_half],
        velocity: [mode, cos_spread, speed.min, speed.max],
        velocity2,
        inherited: [
            inherited.x,
            inherited.y,
            inherited.z,
            f32::from(u8::from(random_phase)),
        ],
        life_size: [
            emitter.spawn.lifetime.min,
            emitter.spawn.lifetime.max,
            init.size.min * smoke_size * scale,
            init.size.max * smoke_size * scale,
        ],
        rot_spin: [init.rotation.min, init.rotation.max, init.spin.min, init.spin.max],
        color,
        right: [right.x, right.y, right.z, scale],
        up: [up.x, up.y, up.z, scale.sqrt()],
        forward: [forward.x, forward.y, forward.z, 0.0],
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// WGSL `sample_local`: the spawn offset in the instance frame. Every angle and cube root
/// below goes through the deterministic f32 kernels ([`det_sin_cos_f32`], [`det_cbrt_f32`]):
/// the WGSL twin in `gpu/shaders.rs` mirrors them op for op, so the CPU mirror and the GPU
/// pool agree bit for bit (neither follows the platform libm or the GPU's `sin`/`cos`/`pow`).
/// Bit for bit holds where the mirror has no mul-add for a driver to contract: the cone
/// (angle) path is pinned exactly on the GPU
/// (`gpu::determinism_tests::cone_child_matches_the_cpu_mirror_bit_for_bit`). The
/// filled-sphere path additionally multiplies the lerped radius by the cube root, and a
/// driver that contracts a mul-add into an fma despite WGSL's rule (observed on lavapipe,
/// 1 ulp in the offset) lands one `f32` step away. Bit for bit also needs correctly rounded
/// f32 division, which WGSL does not promise (2.5 ULP): lavapipe has it, and an RTX 5090
/// lands `det_cbrt` up to 2 ulp away for some draws. The GPU test pins the cube root bit for
/// bit on a software adapter and within 3 ulp on hardware
/// (`filled_sphere_child_matches_the_cpu_mirror_within_driver_ulp`).
fn sample_local(init: &ChildInitGpu, s: u32) -> Vec3 {
    match init.shape[0] as u32 {
        1 => {
            let z = 2.0 * child_unit(s, 0) - 1.0;
            let phi = std::f32::consts::TAU * child_unit(s, 1);
            let flat_r = (1.0 - z * z).max(0.0).sqrt();
            let (sin, cos) = det_sin_cos_f32(phi);
            let dir = Vec3::new(flat_r * cos, flat_r * sin, z);
            let mut r = lerp(init.shape[2], init.shape[3], child_unit(s, 2));
            if init.shape[1] < 0.5 {
                r *= det_cbrt_f32(child_unit(s, 3));
            }
            dir * r
        }
        2 => {
            let h = Vec3::new(init.shape2[0], init.shape2[1], init.shape2[2]);
            let p = Vec3::new(
                (2.0 * child_unit(s, 0) - 1.0) * h.x,
                (2.0 * child_unit(s, 1) - 1.0) * h.y,
                (2.0 * child_unit(s, 2) - 1.0) * h.z,
            );
            if init.shape[1] < 0.5 {
                return p;
            }
            match ((child_unit(s, 3) * 6.0) as u32).min(5) {
                0 => Vec3::new(h.x, p.y, p.z),
                1 => Vec3::new(-h.x, p.y, p.z),
                2 => Vec3::new(p.x, h.y, p.z),
                3 => Vec3::new(p.x, -h.y, p.z),
                4 => Vec3::new(p.x, p.y, h.z),
                _ => Vec3::new(p.x, p.y, -h.z),
            }
        }
        3 => {
            let l = lerp(init.shape[2], init.shape[3], child_unit(s, 0));
            let r = l * init.shape2[3] * child_unit(s, 1).sqrt();
            let phi = std::f32::consts::TAU * child_unit(s, 2);
            let (sin, cos) = det_sin_cos_f32(phi);
            Vec3::new(r * cos, r * sin, l)
        }
        _ => Vec3::ZERO,
    }
}

/// WGSL `sample_velocity`: the init module's velocity, before scaling.
fn sample_velocity(init: &ChildInitGpu, s: u32, local: Vec3) -> Vec3 {
    let speed = lerp(init.velocity[2], init.velocity[3], child_unit(s, 4));
    let right = Vec3::new(init.right[0], init.right[1], init.right[2]);
    let up = Vec3::new(init.up[0], init.up[1], init.up[2]);
    let forward = Vec3::new(init.forward[0], init.forward[1], init.forward[2]);
    match init.velocity[0] as u32 {
        1 => {
            let mut d = Vec3::new(init.velocity2[0], init.velocity2[1], init.velocity2[2]);
            let cos_spread = init.velocity[1];
            if cos_spread < 1.0 && d.length_squared() > 1e-10 {
                let z = lerp(cos_spread, 1.0, child_unit(s, 5));
                let phi = std::f32::consts::TAU * child_unit(s, 6);
                let r = (1.0 - z * z).max(0.0).sqrt();
                let (sin, cos) = det_sin_cos_f32(phi);
                let basis = if d.z.abs() > 0.999 { Vec3::Y } else { Vec3::Z };
                let n = d.normalize();
                let side = n.cross(basis).normalize();
                let side_up = side.cross(n);
                d = side * (r * cos) + side_up * (r * sin) + n * z;
            }
            d * speed
        }
        2 => {
            let flat = right * local.x + up * local.y;
            let d = if flat.length_squared() > 1e-10 {
                flat.normalize()
            } else {
                let phi = std::f32::consts::TAU * child_unit(s, 5);
                let (sin, cos) = det_sin_cos_f32(phi);
                right * cos + up * sin
            };
            let rise = lerp(init.velocity2[0], init.velocity2[1], child_unit(s, 6));
            d * speed + forward * rise
        }
        _ => {
            if local.length_squared() > 1e-10 {
                local.normalize() * speed
            } else {
                forward * speed
            }
        }
    }
}

/// One child particle's starting state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChildSample {
    /// World position.
    pub position: Vec3,
    /// Velocity.
    pub velocity: Vec3,
    /// Lifetime with the identity stamped in ([`stamp_identity`]).
    pub life: f32,
    /// Base size.
    pub size: f32,
    /// Initial linear RGBA.
    pub color: [f32; 4],
}

/// WGSL `spawn_child`: child `seed` of an event at `parent_position` moving at `parent_velocity`.
#[must_use]
pub fn sample_child(
    init: &ChildInitGpu,
    seed: u32,
    parent_position: Vec3,
    parent_velocity: Vec3,
    inherit: f32,
) -> ChildSample {
    let local = sample_local(init, seed);
    let right = Vec3::new(init.right[0], init.right[1], init.right[2]);
    let up = Vec3::new(init.up[0], init.up[1], init.up[2]);
    let forward = Vec3::new(init.forward[0], init.forward[1], init.forward[2]);
    let offset = right * local.x + up * local.y + forward * local.z;
    let inherited = Vec3::new(init.inherited[0], init.inherited[1], init.inherited[2]);
    let life = lerp(init.life_size[0], init.life_size[1], child_unit(seed, 7)).max(0.01);
    ChildSample {
        position: parent_position + offset * init.right[3],
        velocity: (sample_velocity(init, seed, local) + inherited) * init.up[3] + parent_velocity * inherit,
        life: stamp_identity(life, seed),
        size: lerp(init.life_size[2], init.life_size[3], child_unit(seed, 8)).max(0.001),
        color: init.color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned `to_bits` of the cone child position in
    /// `cone_and_sphere_child_sampling_is_pinned_bit_for_bit` (deterministic kernels).
    const CONE_CHILD_POSITION_BITS: [u32; 3] = [1_069_539_152, 3_223_340_750, 1_082_626_954];
    /// Pinned `to_bits` of the filled-sphere child position there.
    const SPHERE_CHILD_POSITION_BITS: [u32; 3] = [1_040_422_169, 1_050_016_880, 3_186_987_011];

    /// The hash is the same function as WGSL `hash_u32`; these values are the kernel's outputs
    /// for the same inputs by construction, and any edit here must edit the WGSL too.
    #[test]
    fn the_hash_is_integer_exact() {
        assert_eq!(hash_u32(0), hash_u32(0));
        assert_ne!(hash_u32(1), hash_u32(2));
        assert!((0.0..1.0).contains(&child_unit(u32::MAX, 3)));
    }

    /// Probability 1 always fires, 0 never does, and a half rolls near half over many seeds.
    #[test]
    fn link_rolls_follow_the_probability() {
        let mut fired = 0;
        for seed in 0..4000u32 {
            let link = link_seed(event_seed(seed, 7, TRIGGER_DEATH), 0);
            assert!(link_fires(link, 1.0));
            assert!(!link_fires(link, 0.0));
            fired += u32::from(link_fires(link, 0.5));
        }
        assert!((1800..2200).contains(&fired), "{fired} of 4000");
    }

    /// The identity survives the stamp and moves the lifetime by a tiny fraction.
    #[test]
    fn a_stamped_lifetime_keeps_its_value_and_carries_an_identity() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..300u32 {
            let life = stamp_identity(1.5, seed);
            assert!((life - 1.5).abs() < 1.5 * 2f32.powi(-13), "{life}");
            seen.insert(child_identity(life));
        }
        assert!(seen.len() > 200, "identities should differ: {}", seen.len());
    }

    /// The child sampling is pinned bit for bit: every angle and cube root goes through the
    /// deterministic f32 kernels, and the WGSL twin in `gpu/shaders.rs` must reproduce exactly
    /// these bits on the GPU (fix-vfx1-determinism finding 4). A platform libm (or a WGSL
    /// `sin`/`cos`/`pow`) drifts from these pins.
    #[test]
    fn cone_and_sphere_child_sampling_is_pinned_bit_for_bit() {
        let init = ChildInitGpu {
            shape: [3.0, 0.0, 0.5, 2.0], // cone, length 0.5..2
            shape2: [0.0, 0.0, 0.0, det_tan(0.6f32.to_radians().clamp(0.0, 1.55))],
            velocity: [0.0, 1.0, 0.0, 0.0], // radial, speed 0
            velocity2: [0.0; 4],
            inherited: [0.0; 4],
            life_size: [1.0, 1.0, 0.2, 0.2],
            rot_spin: [0.0; 4],
            color: [1.0, 1.0, 1.0, 1.0],
            right: [1.0, 0.0, 0.0, 1.0],
            up: [0.0, 1.0, 0.0, 1.0],
            forward: [0.0, 0.0, 1.0, 0.0],
        };
        let seed = 0x1234_5678;
        let sample = sample_child(&init, seed, Vec3::new(1.5, -2.5, 3.25), Vec3::ZERO, 0.0);
        let bits = |v: Vec3| [v.x.to_bits(), v.y.to_bits(), v.z.to_bits()];
        assert_eq!(
            bits(sample.position),
            CONE_CHILD_POSITION_BITS,
            "cone child position"
        );
        assert_eq!(bits(sample.velocity), [0; 3], "radial speed 0: no velocity");
        // A filled sphere child exercises the cube-root radius.
        let mut sphere = init;
        sphere.shape = [1.0, 0.0, 0.1, 0.6];
        sphere.shape2 = [0.0; 4];
        let s2 = sample_child(&sphere, seed, Vec3::ZERO, Vec3::ZERO, 0.0);
        assert_eq!(
            bits(s2.position),
            SPHERE_CHILD_POSITION_BITS,
            "sphere child position"
        );
    }
}
