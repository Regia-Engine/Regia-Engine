// SPDX-License-Identifier: MPL-2.0
//! Spec 73 § 14: what one character's re-run costs. One character walks on rapier colliders, and
//! the run times eight `step_character` calls in a row (a correction over a 133 ms lead), through
//! `RapierCharacterQuery`.
//!
//! `cargo run --release -p regia_framework --features net --example net_character_bench -- 2000`

use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{
    CharacterInput, CharacterPlugin, CharacterState, RapierCharacterQuery, step_character,
    use_rapier_character_controller,
};
use regia_framework::net_character::NetCharacter;
use regia_net::NetResimulating;
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

const RESIM_TICKS: usize = 8;
const DT: f32 = 1.0 / 60.0;

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2000);
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(CharacterPlugin)
        .insert_resource(NetResimulating(true));
    use_rapier_character_controller(&mut app);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(500.0, 0.5, 500.0),
            },
            ..Collider::default()
        },
    ));
    let start = Vec3::new(0.0, 1.21, 0.0);
    let hero = app
        .world_mut()
        .spawn((
            Transform::from_translation(start),
            NetCharacter::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
        ))
        .id();
    for _ in 0..3 {
        app.update();
    }
    let input = CharacterInput {
        move_dir: Vec3::X,
        ..CharacterInput::default()
    };
    let mut times: Vec<Duration> = Vec::with_capacity(rounds);
    let world = app.world_mut();
    for _ in 0..rounds {
        let began = Instant::now();
        for _ in 0..RESIM_TICKS {
            step_character(world, &RapierCharacterQuery, hero, &input, DT);
        }
        times.push(began.elapsed());
        if let Some(mut t) = world.get_mut::<Transform>(hero) {
            t.translation = start;
        }
    }
    times.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mean: f64 = times.iter().map(|d| ms(*d)).sum::<f64>() / rounds.max(1) as f64;
    let p99 = times[(rounds * 99 / 100).min(rounds.saturating_sub(1))];
    println!(
        "re-run of {RESIM_TICKS} ticks, 1 character, rapier: mean {mean:.4} ms, p99 {:.4} ms ({rounds} rounds)",
        ms(p99)
    );
}
