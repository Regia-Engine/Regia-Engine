// SPDX-License-Identifier: MPL-2.0
//! Client interpolation cost per frame (spec 74 § 14): `net_interpolate` for N proxies moving on
//! a circle, states every 2 ticks at 60 Hz, frames at 60 Hz, no network (states are pushed into
//! the buffers directly, as the client apply does).
//!
//! `cargo run --release -p regia_net --example net_interp_bench -- [proxies] [frames]` (defaults
//! 500, 600). Prints the mean and worst time of one `net_interpolate` call.

use std::time::Instant;

use glam::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_net::interp::{NetSnapshotBuffer, ProxyState, SnapshotBuffer, install_interpolation};
use regia_net::*;
use regia_scene::transform::Transform;

fn state(i: usize, tick: u64) -> ProxyState {
    let a = tick as f32 / 60.0 + i as f32;
    ProxyState {
        tick,
        translation: Vec3::new(10.0 * a.cos(), 0.0, 10.0 * a.sin()),
        rotation: Quat::from_rotation_y(a),
        scale: Vec3::ONE,
        velocity: Some(Vec3::new(-10.0 * a.sin(), 0.0, 10.0 * a.cos())),
        teleport: false,
        steady: false,
    }
}

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let proxies = args.first().copied().unwrap_or(500);
    let frames = args.get(1).copied().unwrap_or(600);
    let mut world = World::new();
    world.insert_resource(NetMode::Client);
    install_interpolation(&mut world, 60.0, 2.0);
    let ids: Vec<Entity> = (0..proxies)
        .map(|_| {
            world
                .spawn((Transform::default(), NetSnapshotBuffer(SnapshotBuffer::new(32))))
                .id()
        })
        .collect();
    let mspt = 1000.0 / 60.0;
    let mut times = Vec::with_capacity(frames);
    for f in 0..frames {
        let now = f as f64 * mspt;
        if f.is_multiple_of(2) {
            let tick = f as u64;
            world
                .resource_mut::<NetInterpolation>()
                .clock
                .on_state(tick, now + 30.0);
            for (i, e) in ids.iter().enumerate() {
                if let Some(mut b) = world.get_mut::<NetSnapshotBuffer>(*e) {
                    b.0.push(state(i, tick));
                }
            }
        }
        let t0 = Instant::now();
        net_interpolate(&mut world, now + 30.0);
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let warm = &times[times.len().min(60)..];
    let mean = warm.iter().sum::<f64>() / warm.len().max(1) as f64;
    let worst = warm.iter().copied().fold(0.0, f64::max);
    println!("net_interpolate: {proxies} proxies, {frames} frames: mean {mean:.3} ms, worst {worst:.3} ms");
}
