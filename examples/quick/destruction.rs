// SPDX-License-Identifier: MPL-2.0
//! Destruction level 2 sample (ADR-1597): a brick wall and a plank bridge on two abutments, broken
//! by a fixed hit script (a shot through the wall, a blast against it, a blast that cuts the deck,
//! hits that drop the deck's left half). One fixed physics tick per frame; the scene restarts a few
//! seconds after the script ends. `cargo run -p regia --example destruction --features quick`
//!
//! With `REGIA_DESTRUCTION_CAPTURE=<file.png>` it saves the frame after the script and the debris
//! settle, prints the break model's hashes, and quits: the hashes are the same on lavapipe, any GPU
//! and any platform (the break decisions never touch the renderer), and match
//! `crates/regia_physics/tests/destruction.rs`.

use regia::physics::PhysicsWorld;
use regia::physics::destruction::sample::{SAMPLE_TICKS, define_sample, sample_hits};
use regia::physics::destruction::{
    Authority, ChunkState, DestructionPhysics, DestructionWorld, add_fixed_box, step_standalone,
};
use regia::quick::prelude::*;

/// Fixed tick length (one tick per frame).
const DT: f32 = 1.0 / 60.0;
/// Ticks after the script before the capture (debris settles).
const SETTLE_TICKS: u64 = 120;
/// Ticks after the script before the demo restarts.
const RESTART_TICKS: u64 = 300;

#[derive(Default)]
struct Sim {
    world: DestructionWorld,
    bridge: DestructionPhysics,
    physics: PhysicsWorld,
    ids: Option<regia::physics::destruction::sample::SampleIds>,
}

#[derive(Reflect, Default)]
struct Scene {
    tick: u64,
    attached: u32,
    debris: u32,
    capture: Option<String>,
    captured: bool,
    #[reflect(ignore)]
    sim: Sim,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("Destruction")
        .size(1280, 720)
        .run();
}

fn model(_q: &Quick) -> Scene {
    Scene {
        capture: std::env::var("REGIA_DESTRUCTION_CAPTURE").ok(),
        sim: new_sim(),
        ..Scene::default()
    }
}

/// The world, the two sample instances, and fixed colliders for the ground and the abutments.
fn new_sim() -> Sim {
    let mut sim = Sim {
        world: DestructionWorld::new(Authority::Server),
        ..Sim::default()
    };
    let ids = match define_sample(&mut sim.world) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("sample: {e}");
            return sim;
        }
    };
    for id in [ids.wall, ids.bridge] {
        if let Err(e) = sim.bridge.attach(&mut sim.physics, &sim.world, id) {
            eprintln!("attach: {e}");
        }
    }
    sim.ids = Some(ids);
    for (centre, half) in [
        ([0.0, -0.5, 0.0], [40.0, 0.5, 40.0]),
        ([-5.5, 1.5, 5.0], [0.5, 1.5, 0.5]),
        ([5.5, 1.5, 5.0], [0.5, 1.5, 0.5]),
    ] {
        add_fixed_box(&mut sim.physics, centre, half);
    }
    sim
}

fn update(q: &Quick, m: &mut Scene) {
    let sim = &mut m.sim;
    let Some(ids) = sim.ids else {
        return;
    };
    let tick = sim.world.tick();
    for hit in sample_hits(tick + 1, ids) {
        if let Err(e) = sim.world.damage(hit) {
            eprintln!("damage: {e}");
        }
    }
    sim.world.step(DT);
    sim.bridge.sync(&mut sim.physics, &mut sim.world, DT);
    step_standalone(&mut sim.physics, DT, [0.0, -9.81, 0.0]);
    m.tick = sim.world.tick();
    m.attached = sim
        .world
        .instances()
        .map(|(_, i)| i.attached_count() as u32)
        .sum();
    m.debris = sim.bridge.stats().active;
    if let Some(path) = m.capture.clone() {
        if m.tick == SAMPLE_TICKS + SETTLE_TICKS && !m.captured {
            q.save_frame(&path);
            m.captured = true;
            println!(
                "DESTRUCTION_QUICK tick={} structure={:#018x} state={:#018x} events={} capture={path}",
                m.tick,
                sim.world.structure_hash(),
                sim.world.state_hash(),
                sim.world.event_count()
            );
        } else if m.captured {
            q.quit();
        }
    } else if m.tick >= SAMPLE_TICKS + RESTART_TICKS {
        m.sim = new_sim();
    }
}

fn view(q: &Quick, m: &Scene) {
    // No 2D background: it would cover the 3D view (the sky is the renderer's).
    q.camera_3d(Vec3::new(5.0, 9.0, 14.0), Vec3::new(-0.5, 1.2, -1.0));
    let d3 = q.draw_3d();
    d3.plane(Vec3::ZERO, Vec2::new(40.0, 40.0))
        .color(srgb_u8(120, 125, 115))
        .roughness(0.9);
    for x in [-5.5, 5.5] {
        d3.cube(Vec3::new(x, 1.5, 5.0), Vec3::new(1.0, 3.0, 1.0))
            .color(srgb_u8(140, 140, 135))
            .roughness(0.8);
    }
    let sim = &m.sim;
    for (id, inst) in sim.world.instances() {
        let asset = inst.asset();
        let colour = if asset.material.fx_tag == "wood" {
            srgb_u8(125, 88, 52)
        } else {
            srgb_u8(150, 68, 48)
        };
        let pose = inst.pose();
        for (c, chunk) in asset.chunks.iter().enumerate() {
            let size = Vec3::new(
                chunk.bounds_max[0] - chunk.bounds_min[0],
                chunk.bounds_max[1] - chunk.bounds_min[1],
                chunk.bounds_max[2] - chunk.bounds_min[2],
            ) * 0.98;
            if inst.chunk_states()[c] == ChunkState::Attached {
                let centre = Vec3::from_array(pose.transform_point(chunk.centroid));
                d3.cube(centre, size).color(colour).roughness(0.85);
            } else if let Some((at, rotation)) = sim.bridge.debris_pose(&sim.physics, id, c as u32) {
                d3.cube(at, size).rotation(rotation).color(colour).roughness(0.85);
            }
        }
    }
    let area = q.window_rect();
    q.draw()
        .text(&format!(
            "tick {}  attached {}  debris {}  events {}",
            m.tick,
            m.attached,
            m.debris,
            sim.world.event_count()
        ))
        .xy(Vec2::new(area.left() + 220.0, area.top() - 20.0))
        .font_size(18)
        .color(srgb_u8(20, 20, 30));
}
