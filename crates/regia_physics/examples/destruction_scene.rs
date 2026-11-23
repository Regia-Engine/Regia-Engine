// SPDX-License-Identifier: MPL-2.0
//! Destruction level 2 sample scene, headless (ADR-1597): the brick wall and the plank bridge on a
//! ground plane, broken by the fixed hit script with rapier running. Prints the break model's
//! hashes (they must equal the pinned values in `tests/destruction.rs` on every platform) and the
//! debris pool's counters.
//!
//! `cargo run -p regia_physics --example destruction_scene [ticks]`
//!
//! `--perf` instead times the break model and the rapier sync on a 16 x 16 x 16 grid (4096 chunks,
//! 11,520 bonds) hit by one large blast, with the default caps, and prints per-tick costs.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_physics::destruction::sample::{
    BRIDGE_POSE, SAMPLE_SEEDS, SAMPLE_TICKS, SampleIds, WALL_POSE, run_sample_headless, sample_assets,
    sample_hits,
};
use regia_physics::destruction::{DestructiblePose, Destruction, DestructionPlugin};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody};
use regia_scene::Transform;

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "--perf") {
        return perf();
    }
    let ticks = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<u64>().ok())
        .unwrap_or(SAMPLE_TICKS + 240);
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, DestructionPlugin::default()));
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(0.0, -0.5, 0.0),
            ..Transform::default()
        },
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(50.0, 0.5, 50.0),
            },
            ..Collider::default()
        },
    ));
    let Ok((wall, bridge)) = sample_assets() else {
        eprintln!("the sample assets failed to build");
        return ExitCode::FAILURE;
    };
    let defined = app
        .world_mut()
        .resource_scope(|world, mut d: regia_ecs::prelude::Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            let wall = d.define(
                &mut physics,
                Arc::clone(&wall),
                DestructiblePose::at(WALL_POSE),
                SAMPLE_SEEDS[0],
            )?;
            let bridge = d.define(
                &mut physics,
                Arc::clone(&bridge),
                DestructiblePose::at(BRIDGE_POSE),
                SAMPLE_SEEDS[1],
            )?;
            Ok::<_, regia_physics::destruction::DestructionError>(SampleIds { wall, bridge })
        });
    let ids = match defined {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("define failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut updates = 0u64;
    loop {
        let tick = app.world().resource::<Destruction>().world.tick();
        if tick >= ticks || updates > ticks * 4 + 100 {
            break;
        }
        for hit in sample_hits(tick + 1, ids) {
            if let Err(e) = app.world_mut().resource_mut::<Destruction>().world.damage(hit) {
                eprintln!("damage failed: {e}");
                return ExitCode::FAILURE;
            }
        }
        while app.world().resource::<Destruction>().world.tick() == tick && updates <= ticks * 4 + 100 {
            app.update();
            updates += 1;
        }
    }
    let d = app.world().resource::<Destruction>();
    let s = d.physics.stats();
    println!(
        "DESTRUCTION_SCENE ticks={} structure={:#018x} state={:#018x} events={} attached=[{}, {}]",
        d.world.tick(),
        d.world.structure_hash(),
        d.world.state_hash(),
        d.world.event_count(),
        d.world.instance(ids.wall).map_or(0, |i| i.attached_count()),
        d.world.instance(ids.bridge).map_or(0, |i| i.attached_count()),
    );
    println!(
        "DESTRUCTION_POOL active={} pool_free={} spawned={} reuses={} despawned={} evicted={} colliders_removed={} last_sync_us={}",
        s.active,
        s.pool_free,
        s.spawned_total,
        s.pool_reuses,
        s.despawned_total,
        s.evicted_total,
        s.colliders_removed_total,
        s.last_sync_us
    );
    // The break model alone must agree with the full run (rapier never feeds back).
    match run_sample_headless(SAMPLE_TICKS) {
        Ok(headless) if d.world.event_count() == headless.events.len() as u64 => ExitCode::SUCCESS,
        Ok(headless) => {
            eprintln!(
                "event count {} differs from the headless run's {}",
                d.world.event_count(),
                headless.events.len()
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("headless run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Times one large blast on a 4096-chunk grid: the break model's step and the rapier sync, per tick.
fn perf() -> ExitCode {
    use regia_physics::destruction::{
        Authority, BoxGrid, Damage, DamageKind, DestructibleAsset, DestructionBudget, DestructionMaterial,
        DestructionPhysics, DestructionWorld, GridSupport,
    };
    let asset = match DestructibleAsset::box_grid(&BoxGrid {
        name: "perf_grid".into(),
        counts: [16, 16, 16],
        cell: [0.5, 0.5, 0.5],
        density: 2000.0,
        strength_per_area: 4000.0,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    }) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("grid: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut world = DestructionWorld::new(Authority::Server);
    world.set_budget(DestructionBudget::default());
    let mut physics = PhysicsWorld::default();
    let mut bridge = DestructionPhysics::default();
    let defined = world
        .define(asset, DestructiblePose::default(), 1)
        .and_then(|id| bridge.attach(&mut physics, &world, id).map(|()| id));
    let id = match defined {
        Ok(id) => id,
        Err(e) => {
            eprintln!("define: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = world.damage(Damage {
        target: Some(id),
        kind: DamageKind::Explosion {
            center: [0.0, 4.0, 0.0],
            radius: 2.5,
            impulse: 60_000.0,
        },
    }) {
        eprintln!("damage: {e}");
        return ExitCode::FAILURE;
    }
    let mut total_step = 0u128;
    let mut worst_step = 0u128;
    let mut worst_sync = 0u64;
    for tick in 1..=30u32 {
        let started = std::time::Instant::now();
        let report = world.step(1.0 / 60.0);
        let step_us = started.elapsed().as_micros();
        let sync = bridge.sync(&mut physics, &mut world, 1.0 / 60.0);
        total_step += step_us;
        worst_step = worst_step.max(step_us);
        worst_sync = worst_sync.max(bridge.stats().last_sync_us);
        if tick <= 5 || !report.events.is_empty() {
            println!(
                "DESTRUCTION_PERF tick={tick} events={} deferred={} step_us={step_us} sync_us={} spawned={} active={} pending={}",
                report.events.len(),
                report.deferred_breaks,
                bridge.stats().last_sync_us,
                sync.spawned.len(),
                bridge.stats().active,
                world.pending_spawns(),
            );
        }
    }
    // Idle ticks: nothing hit, nothing over threshold (the bond scan is skipped).
    let started = std::time::Instant::now();
    for _ in 0..60 {
        world.step(1.0 / 60.0);
        bridge.sync(&mut physics, &mut world, 1.0 / 60.0);
    }
    println!(
        "DESTRUCTION_PERF_IDLE mean_tick_us={}",
        started.elapsed().as_micros() / 60
    );
    println!(
        "DESTRUCTION_PERF_SUMMARY chunks=4096 bonds={} detached={} mean_step_us={} worst_step_us={worst_step} worst_sync_us={worst_sync}",
        world.instance(id).map_or(0, |i| i.asset().bonds.len()),
        world.counters().chunks_detached,
        total_step / 30,
    );
    ExitCode::SUCCESS
}
