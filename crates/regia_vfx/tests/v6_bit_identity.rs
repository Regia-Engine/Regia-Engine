// SPDX-License-Identifier: MPL-2.0
//! `.vfx.ron` v7 (ADR-2800): a v6 effect migrates and simulates bit-identically.
//!
//! The pinned hash below was recorded by running this exact file against the v6 code (the lane's
//! base, before the v7 change): every spawn job and every probe step of a v6 effect that uses
//! each v6 shape, velocity mode, the wind, `kill_below` and an event link. The v7 code must give
//! the same bits, from the file as written and re-saved as v7.

use regia_core::math::Vec3;
use regia_vfx::format::VfxSystemDef;
use regia_vfx::probe::{ProbeSettings, probe_steps};
use regia_vfx::sim::{LodInput, VfxInstance};

const V6: &str = r##"(
    version: 6,
    name: "v6_pin",
    system: (budget: 20000),
    emitters: [
        (
            name: "sphere", spawn: (mode: Rate(90.0), lifetime: (min: 0.4, max: 1.2)),
            init: (shape: Sphere(radius: (min: 0.2, max: 0.8), surface: true),
                   velocity: (mode: Radial(speed: (min: 1.0, max: 3.0)), inherit: 0.5),
                   size: (min: 0.1, max: 0.3), rotation: (min: 0.0, max: 3.0), spin: (min: -1.0, max: 1.0),
                   color: [(t: 0.0, color: "#ff8040")]),
            update: (gravity: (0.0, -4.0, 0.0), drag: 0.4, kill_below: Some(-0.5), wind_response: 0.6, wind_drag: 2.0,
                     events: [(on: Death, child: "child", count: 2, probability: 0.5, inherit_velocity: 0.3)]),
            render: (mode: Billboard, blend: Alpha, flipbook: Some((cols: 4, rows: 2, frames: 8, mode: OverLife, random_start: true))),
        ),
        (
            name: "box", spawn: (mode: Burst(64), lifetime: (min: 0.5, max: 0.9)),
            init: (shape: Box(half_extents: (1.0, 0.5, 2.0), surface: true),
                   velocity: (mode: Directional(dir: (0.0, 1.0, 0.2), spread: 20.0, speed: (min: 2.0, max: 4.0))),
                   size: (min: 0.2, max: 0.2)),
            update: (gravity: (0.0, -9.8, 0.0)),
            render: (mode: Stretched),
        ),
        (
            name: "cone", spawn: (mode: PerDistance(every: 0.25), lifetime: (min: 0.3, max: 0.6)),
            init: (shape: Cone(angle: 30.0, length: (min: 0.1, max: 0.5)),
                   velocity: (mode: Planar(speed: (min: 3.0, max: 5.0), lift: (min: 0.5, max: 1.0))),
                   size: (min: 0.1, max: 0.4)),
            render: (mode: Mesh),
        ),
        (
            name: "volume", spawn: (mode: Rate(40.0), lifetime: (min: 1.0, max: 1.0)),
            init: (shape: Box(half_extents: (0.3, 0.3, 0.3)), size: (min: 0.1, max: 0.1)),
            update: (wind_response: 1.0),
            render: (mode: Billboard),
        ),
        (
            name: "child", spawn: (mode: Event, lifetime: (min: 0.5, max: 1.0)),
            init: (shape: Sphere(radius: (min: 0.0, max: 0.2)), size: (min: 0.1, max: 0.1)),
            update: (gravity: (0.0, -2.0, 0.0)),
            render: (mode: Billboard),
        ),
    ],
)"##;

/// FNV-1a over 32-bit words.
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

/// The spawn jobs of 120 frames of a moving instance, then 90 probe steps (CPU reference with
/// wind and events), all as bits.
fn run(def: &VfxSystemDef) -> u64 {
    let mut words = Vec::new();
    let mut instance = VfxInstance::new(0, 41, Vec3::new(1.0, 2.0, 3.0), Vec3::new(0.2, 1.0, 0.1));
    instance.set_scale(1.5);
    instance.set_intensity(0.8);
    let mut jobs = Vec::new();
    for frame in 0..120 {
        let t = frame as f32 / 60.0;
        instance.set_transform(Vec3::new(t * 4.0, 2.0, 3.0), Vec3::new(0.2, 1.0, 0.1), 1.0 / 60.0);
        instance.schedule(
            def,
            1.0 / 60.0,
            &LodInput {
                camera_pos: Vec3::ZERO,
            },
            u32::MAX,
        );
        jobs.clear();
        instance.drain_jobs(&mut jobs);
        for job in &jobs {
            words.extend(
                job.position
                    .iter()
                    .chain(&job.velocity)
                    .chain(&job.color)
                    .map(|v| v.to_bits()),
            );
            words.extend([job.lifetime, job.size, job.rotation, job.spin, job.phase].map(f32::to_bits));
        }
    }
    let settings = ProbeSettings {
        wind: Vec3::new(3.0, 0.0, -1.0),
        ground: Some(0.0),
        ..ProbeSettings::default()
    };
    let (states, stats) = probe_steps(def, Vec3::new(0.0, 1.0, 0.0), Vec3::Y, settings, 90);
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
    fnv(words)
}

/// Recorded on the v6 code (base 9a4301b83) with this file.
///
/// Re-pinned by the fix-vfx1-determinism lane (intended change, recorded for the lead): the old
/// pin encoded spawn sampling drawn from the platform libm (`f32::sin/cos/tan/cbrt` in `sim.rs`),
/// which differs between glibc, the Windows CRT and musl. The sampling now runs on the
/// deterministic det kernels, so the pin moved — the new bits are the same on every platform.
/// Integration batch 4 confirmed the transition with the same host and final core math:
/// the VFX tree at `5bf958b70^1` gives `d2628dc67e173408`, while `5bf958b70` and the
/// final tree give `6d373f315bb2da27`. Each tree's unchanged regression passed, including
/// the re-save check; this isolates the movement to the VFX1 merge.
const V6_PIN: u64 = 0x6d37_3f31_5bb2_da27;

#[test]
fn a_v6_effect_simulates_bit_identically() {
    let def = VfxSystemDef::from_ron(V6).expect("v6 loads");
    let hash = run(&def);
    println!("v6 pin: {hash:#018x}");
    assert_eq!(hash, V6_PIN, "the v6 effect's jobs and probe moved");
    let resaved = VfxSystemDef::from_ron(&def.to_ron().expect("save")).expect("reload");
    assert_eq!(run(&resaved), hash, "re-saving changes nothing");
}
