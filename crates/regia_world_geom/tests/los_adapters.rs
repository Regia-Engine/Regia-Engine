// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/influence.rs lines 308-345, commit c9513623f, tag
// regia-absorb-4), ADR-8012. Mark: adapt (plan row S-10, ADR-8395): DO27's flat and ridge samplers and its two
// heightfield line-of-sight tests, run through `regia_detsim::ai::LineOfSight` on this crate's copy of DO27's
// sampler (assertions unchanged). The Stage A mouth test and the board tests are new.

//! `LineOfSight` over this crate's Stage A geometry (plan row S-10, ADR-8395): the terrain sampler keeps Direct
//! Order 27's heightfield rule, and the tunnel collision mesh sees through a real tunnel mouth.

#![allow(clippy::unwrap_used)]

use regia_detsim::ai::{
    HeightfieldLos, LineOfSight, PerceptEnemy, PerceptUnit, PerceptionParams, SightSource, SightingsBoard,
    los_clear,
};
use regia_detsim::math;
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;

fn flat_sampler() -> TerrainSampler {
    let grid = HeightGrid::square(64, vec![0u16; 64 * 64]).unwrap();
    TerrainSampler::new(grid, 1280.0)
}

fn ridge_sampler() -> TerrainSampler {
    // A ridge at world z ≈ 160 m (off the map centre, which is zeroed).
    let mut heights = vec![0u16; 64 * 64];
    for z in 0..64usize {
        for x in 0..64usize {
            let dz = (z as i32 - 40).abs();
            let h = if dz < 4 { 640u16 } else { 0u16 }; // 20 m ridge at index z=40
            heights[z * 64 + x] = h;
        }
    }
    let grid = HeightGrid::square(64, heights).unwrap();
    TerrainSampler::new(grid, 1280.0)
}

#[test]
fn los_is_clear_over_flat_ground() {
    let t = flat_sampler();
    assert!(los_clear(Some(&t), [0.0, 0.0, -100.0], [0.0, 0.0, 100.0]));
}

#[test]
fn los_is_blocked_by_a_ridge() {
    let t = ridge_sampler();
    // From the near side to the far side the segment crosses the ridge.
    assert!(!los_clear(Some(&t), [0.0, 0.0, 0.0], [0.0, 0.0, 400.0]));
    // Two points on the near side of the ridge stay clear.
    assert!(los_clear(Some(&t), [0.0, 0.0, 0.0], [0.0, 0.0, 40.0]));
}

/// The sampler's rule is `heightfield_los_clear` over `height_at_world`, answer for answer, on a fan of lines
/// across the ridge (so a game swapping DO27's `los_clear` for the trait keeps every answer).
#[test]
fn the_sampler_rule_is_the_heightfield_rule() {
    let t = ridge_sampler();
    let closure = HeightfieldLos(|x: f32, z: f32| t.height_at_world(x, z));
    let mut blocked = 0;
    for i in 0..64 {
        for j in 0..16 {
            let from = [i as f32 * 9.0 - 300.0, j as f32 * 2.0, -50.0 + j as f32 * 7.0];
            let to = [
                300.0 - i as f32 * 5.0,
                30.0 - j as f32 * 3.0,
                380.0 - i as f32 * 4.0,
            ];
            assert_eq!(t.clear(from, to), closure.clear(from, to), "{from:?} -> {to:?}");
            blocked += usize::from(!t.clear(from, to));
        }
    }
    assert!(
        blocked > 0 && blocked < 64 * 16,
        "the fan crosses and clears the ridge: {blocked}"
    );
}

/// The Switchbacks' west mouth (the fixture layout over its flat canyon floor at -256 m): the ramp climbs from
/// the tube to the opening at (-170, -256, 130). An observer stands on the plain 10 m out from the opening,
/// 2 m up, and looks down the ramp's last leg at a point under the ramp's lid; a second point lies 25 m to the
/// side, in the rock.
struct WestMouth {
    terrain: TerrainSampler,
    tunnels: TunnelsState,
    eye: [f32; 3],
    in_the_tunnel: [f32; 3],
    in_the_rock: [f32; 3],
}

fn west_mouth() -> WestMouth {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let tunnels = TunnelsState::<()>::for_layout(switchbacks(), Some(&terrain));
    // The ramp's last leg: from its bend on the tube floor to the opening on the surface.
    let top = [-170.0f32, -256.0, 130.0];
    let bend = [-178.0f32, -286.0, 96.0];
    let d = [bend[0] - top[0], bend[1] - top[1], bend[2] - top[2]];
    let plan = math::sqrt(d[0] * d[0] + d[2] * d[2]);
    let eye = [
        top[0] - d[0] / plan * 10.0,
        top[1] + 2.0,
        top[2] - d[2] / plan * 10.0,
    ];
    let in_the_tunnel = [
        top[0] + d[0] * 0.9,
        top[1] + d[1] * 0.9 + 2.0,
        top[2] + d[2] * 0.9,
    ];
    let in_the_rock = [in_the_tunnel[0] + 25.0, in_the_tunnel[1], in_the_tunnel[2]];
    WestMouth {
        terrain,
        tunnels,
        eye,
        in_the_tunnel,
        in_the_rock,
    }
}

/// The row's acceptance test on a real Stage A mouth: through the trait, the tunnel mesh sees down the open
/// mouth to a point under the lid and not through the wall; the heightfield alone sees neither (the ground over
/// the tunnel blocks), which is why sight needs the world query and not the heightfield.
#[test]
fn los_trait_sees_through_an_open_mouth() {
    let m = west_mouth();
    let mesh = m.tunnels.mesh.as_ref().unwrap();
    let t = m.in_the_tunnel;
    assert!(
        mesh.ceiling_above(t[0], t[2], t[1] - 2.0).is_some(),
        "the target is under the ramp's lid: a covered tunnel, not the open cut"
    );
    assert!(
        mesh.clear(m.eye, m.in_the_tunnel),
        "the open mouth frames the covered ramp"
    );
    assert!(
        !mesh.clear(m.eye, m.in_the_rock),
        "the ramp wall hides the rock beside it"
    );
    assert!(
        !m.terrain.clear(m.eye, m.in_the_tunnel),
        "the heightfield cannot see into the tunnel"
    );

    let observer = PerceptUnit {
        id: 1,
        team: 1,
        pos: m.eye,
        sight_range_m: 220.0,
        radar_scale: 1.0,
    };
    let enemy = |id: u32, pos: [f32; 3]| PerceptEnemy {
        id,
        team: 2,
        pos,
        vel: [0.0, 0.0],
        dps: 60.0,
        band: [0.0, 150.0],
        lit: false,
    };
    let enemies = [enemy(10, m.in_the_tunnel), enemy(11, m.in_the_rock)];
    let params = [PerceptionParams {
        los_recheck_interval_ticks: 1,
        ..PerceptionParams::default()
    }; 3];
    // Radar 5 m: every sighting below is a direct line of sight.
    let mut board = SightingsBoard::new(3, 5.0);
    board.refresh(mesh, 0, &[observer], &enemies, &params);
    let row = board.for_team(1);
    assert_eq!(
        row.len(),
        1,
        "only the enemy in the mouth's frame is seen: {row:?}"
    );
    assert_eq!(row[0].entity, 10);
    assert_eq!(row[0].sighting.source, SightSource::DirectLos);

    let mut heightfield_only = SightingsBoard::new(3, 5.0);
    heightfield_only.refresh(Some(&m.terrain), 0, &[observer], &enemies, &params);
    assert!(heightfield_only.for_team(1).is_empty());
}
