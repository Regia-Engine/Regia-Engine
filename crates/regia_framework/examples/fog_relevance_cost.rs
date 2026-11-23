// SPDX-License-Identifier: MPL-2.0
//! Time the server's fog relevance per tick: `fog_relevance_prepare` (one sight and one lead vision
//! per team) and the filter asked once per (connection, fogged entity), as the relevance pipeline
//! does on a send tick. Prints numbers; asserts only that each tick built one vision per team:
//! `cargo run --release -p regia_framework --features net --example fog_relevance_cost -- [entities=900]
//! [teams=4] [peers=32] [ticks=200] [half_extent_m=2000]`.

use std::time::Instant;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::fog_relevance::{
    FogRelevance, FogRelevanceFilter, FogRelevanceSettings, FogTeamOf, FogUnit, fog_relevance_prepare,
};
use regia_framework::fog_view::{FogRules, SightClass};
use regia_net::{NetRelevanceFilter, PeerId};
use regia_scene::transform::Transform;

fn arg(i: usize, default: f32) -> f32 {
    std::env::args()
        .nth(i)
        .and_then(|a| a.parse::<f32>().ok())
        .unwrap_or(default)
}

/// A small deterministic generator (SplitMix64) for the positions.
fn next(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

fn main() {
    let entities = arg(1, 900.0) as usize;
    let teams = (arg(2, 4.0) as u32).max(1);
    let peers = (arg(3, 32.0) as u32).max(1);
    let ticks = (arg(4, 200.0) as usize).max(1);
    let half = arg(5, 2000.0);
    let mut world = World::new();
    world.insert_resource(FogRelevance::new(
        FogRules {
            team_count: teams,
            ..FogRules::default()
        },
        FogRelevanceSettings::default(),
    ));
    let mut map = FogTeamOf::default();
    for p in 0..peers {
        map.set(PeerId(p + 1), 1 + p % teams);
    }
    world.insert_resource(map);
    let mut seed = 7u64;
    let mut units = Vec::with_capacity(entities);
    for i in 0..entities {
        let x = (next(&mut seed) * 2.0 - 1.0) * half;
        let z = (next(&mut seed) * 2.0 - 1.0) * half;
        let team = 1 + (i as u32) % teams;
        let class = if i % 10 == 0 {
            SightClass::Building
        } else {
            SightClass::Unit
        };
        let e = world
            .spawn((
                FogUnit::new(team, "unit", class),
                Transform {
                    translation: Vec3::new(x, 0.0, z),
                    ..Transform::default()
                },
            ))
            .id();
        units.push(e);
    }
    let mut schedule = Schedule::default();
    schedule.add_systems(fog_relevance_prepare);
    let (mut prepare_s, mut filter_s, mut shown) = (0.0f64, 0.0f64, 0u64);
    for tick in 0..ticks {
        // Every unit moves half a metre.
        for (i, e) in units.iter().enumerate() {
            if let Some(mut t) = world.get_mut::<Transform>(*e) {
                let dir = if (i + tick / 50).is_multiple_of(2) {
                    0.5
                } else {
                    -0.5
                };
                t.translation.x += dir;
            }
        }
        let start = Instant::now();
        schedule.run(&mut world);
        prepare_s += start.elapsed().as_secs_f64();
        let start = Instant::now();
        for p in 0..peers {
            for e in &units {
                if FogRelevanceFilter::relevant(&world, *e, PeerId(p + 1)) {
                    shown += 1;
                }
            }
        }
        filter_s += start.elapsed().as_secs_f64();
        let status = world.resource::<FogRelevance>().status();
        assert_eq!(status.visions_built, u64::from(teams.min(peers)));
    }
    let per_tick = |s: f64| s * 1000.0 / ticks as f64;
    println!(
        "fog_relevance_cost: entities {entities}, teams {teams}, peers {peers}, ticks {ticks}, half extent {half} m"
    );
    println!(
        "  prepare (visions + linger/lead): {:.3} ms/tick",
        per_tick(prepare_s)
    );
    println!(
        "  filter ({} calls/tick): {:.3} ms/tick",
        u64::from(peers) * entities as u64,
        per_tick(filter_s)
    );
    println!(
        "  shown per peer per tick: {:.1} of {entities}",
        shown as f64 / (ticks as f64 * f64::from(peers))
    );
}
