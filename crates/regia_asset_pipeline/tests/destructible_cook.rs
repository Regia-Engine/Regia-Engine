// SPDX-License-Identifier: MPL-2.0
//! Fracture set → `.destructible.ron` (ADR-1597): the cooked asset validates, round-trips, keeps
//! every chunk and joint, gets its supports from the rule, and breaks deterministically.

use std::sync::Arc;

use regia_asset_pipeline::destructible_cook::{CookSettings, SupportRule, cook_destructible};
use regia_asset_pipeline::fracture::{FractureSettings, box_mesh, fracture_mesh};
use regia_physics::destruction::{
    Authority, Damage, DamageKind, DestructibleAsset, DestructiblePose, DestructionWorld,
};

fn slab() -> regia_asset_pipeline::fracture::FractureSet {
    // A 2 x 1 x 0.5 m slab standing on y = -0.5, split into 24 chunks.
    let (positions, indices) = box_mesh([1.0, 0.5, 0.25]).expect("box");
    fracture_mesh(&positions, &indices, &FractureSettings::random(24, 7)).expect("fracture")
}

#[test]
fn cooked_asset_keeps_chunks_and_joints_and_round_trips() {
    let set = slab();
    let mut settings = CookSettings::new("slab");
    settings.meshes = Some("props/slab.fracture.ron".into());
    let asset = cook_destructible(&set, &settings).expect("cook");
    assert_eq!(asset.chunks.len(), set.chunks.len());
    assert_eq!(asset.bonds.len(), set.joints.len());
    let volume: f64 = set.chunks.iter().map(|c| f64::from(c.volume)).sum();
    assert!(
        (asset.total_mass() - volume * 2400.0).abs() < 1.0,
        "{}",
        asset.total_mass()
    );
    for (bond, joint) in asset.bonds.iter().zip(&set.joints) {
        assert_eq!((bond.a, bond.b), (joint.a, joint.b));
        assert!((bond.break_impulse - joint.break_force * 0.01).abs() <= bond.break_impulse * 1e-6);
    }
    let supports = asset.support_chunks();
    assert!(
        !supports.is_empty() && supports.len() < asset.chunks.len(),
        "{supports:?}"
    );
    for &s in &supports {
        assert!(asset.chunks[s as usize].bounds_min[1] <= -0.49);
    }
    let text = asset.to_ron().expect("ron");
    assert_eq!(DestructibleAsset::from_ron(&text).expect("parse"), asset);
    let again = cook_destructible(&slab(), &settings).expect("cook again");
    assert_eq!(again.to_ron().expect("ron"), text, "same input, same bytes");
}

#[test]
fn cooked_asset_breaks_deterministically() {
    let asset = Arc::new(cook_destructible(&slab(), &CookSettings::new("slab")).expect("cook"));
    let run = || {
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(asset.clone(), DestructiblePose::at([0.0, 0.5, 0.0]), 3)
            .expect("define");
        w.damage(Damage {
            target: Some(id),
            kind: DamageKind::Explosion {
                center: [0.6, 0.9, 0.4],
                radius: 0.8,
                impulse: 50_000.0,
            },
        })
        .expect("queue");
        for _ in 0..4 {
            w.step(1.0 / 60.0);
        }
        (w.state_hash(), w.instance(id).expect("inst").attached_count())
    };
    let (hash, attached) = run();
    assert_eq!(run(), (hash, attached));
    assert!(attached < asset.chunks.len(), "the blast broke chunks off");
}

#[test]
fn cook_rejects_bad_settings() {
    let set = slab();
    let mut s = CookSettings::new("slab");
    s.density = 0.0;
    assert_eq!(
        cook_destructible(&set, &s).expect_err("density").code(),
        "destruction_invalid_asset"
    );
    let mut s = CookSettings::new("slab");
    s.bond_seconds = f32::NAN;
    assert!(cook_destructible(&set, &s).is_err());
    let mut s = CookSettings::new("slab");
    s.support = SupportRule::Explicit(vec![999]);
    assert!(cook_destructible(&set, &s).is_err());
    let mut s = CookSettings::new("slab");
    s.support = SupportRule::None;
    assert!(
        cook_destructible(&set, &s)
            .expect("free")
            .support_chunks()
            .is_empty()
    );
}

/// Mutated `.destructible.ron` text never panics the loader or slips past validation into a world:
/// every mutant either fails with a message or loads and defines cleanly.
#[test]
fn destructible_loader_survives_mutation_fuzzing() {
    let fixture = cook_destructible(&slab(), &CookSettings::new("slab"))
        .expect("cook")
        .to_ron()
        .expect("ron");
    regia_test_utils::fuzz::fuzz_text_loader(&fixture, 0x1597, 150, |text: &str| {
        let asset = DestructibleAsset::from_ron(text)?;
        // Anything that loads must also be accepted by a world.
        let mut w = DestructionWorld::new(Authority::Server);
        w.define(Arc::new(asset), DestructiblePose::default(), 1)
            .map_err(|e| regia_physics::destruction::DestructibleError::Parse(e.to_string()))?;
        Ok::<_, regia_physics::destruction::DestructibleError>(())
    });
}
