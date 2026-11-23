// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/fog_of_war.rs lines 24-62 and 119-176,
// ir-sim/tests/satellite_fog.rs lines 220-285, ir-sim/tests/ai_core.rs lines 89-103;
// commit c9513623f, tag regia-absorb-4), ADR-8012. Mark: adapt (plan row S-10). The board-only tests keep
// DO27's assertions unchanged. Named changes: the flat heightfield is a `HeightfieldLos` over a zero height
// (DO27's `TerrainSampler` lives in `regia_world_geom`; its adapter is tested there); the
// weapon table values are inlined (DO27's weapon stems are game content); `DifficultyProfile::medium()
// .perception` is `PerceptionParams::default()` (the same numbers); DO27's whole-pipeline determinism test
// (`pipeline_is_deterministic_over_ticks`, its board half; S-09 ported the decision half) becomes
// `board_refresh_is_deterministic_over_ticks` over the board alone. The tests that need DO27's game state
// (towers, satellite view, seats) stay in DO27. `los_trait_sees_through_an_open_mouth` and the pins are new.

//! Perception and the line-of-sight seam (plan row S-10, ADR-8395): the team sighting board with decay, and
//! the `LineOfSight` trait a world-geometry query plugs into.

use regia_detsim::ai::{
    HeightfieldLos, INFLUENCE_CELL_M, InfluenceGrid, LineOfSight, LosFn, MouthWall, OpenField, PerceptEnemy,
    PerceptUnit, PerceptionParams, RADAR_RANGE_M, SightSource, SightingsBoard, flank_bearing, line_of_fire,
};

const SIGHT_M: f32 = 220.0;
const TEAMS: usize = 5;
/// DO27's default direct-fire weapon (60 dps, a 0-150 m band), inlined.
const DPS: f32 = 60.0;
const BAND: [f32; 2] = [0.0, 150.0];

/// Flat ground so direct line of sight is purely range-gated.
fn flat() -> HeightfieldLos<impl Fn(f32, f32) -> f32> {
    HeightfieldLos(|_x: f32, _z: f32| 0.0)
}

fn params() -> Vec<PerceptionParams> {
    // Re-check LOS every tick so tick-0 sightings are deterministic.
    let p = PerceptionParams {
        los_recheck_interval_ticks: 1,
        ..PerceptionParams::default()
    };
    vec![p; TEAMS]
}

fn observer(id: u32, team: u8, x: f32) -> PerceptUnit {
    PerceptUnit {
        id,
        team,
        pos: [x, 0.0, 0.0],
        sight_range_m: SIGHT_M,
        radar_scale: 1.0,
    }
}

fn foe(id: u32, team: u8, x: f32) -> PerceptEnemy {
    PerceptEnemy {
        id,
        team,
        pos: [x, 0.0, 0.0],
        vel: [0.0, 0.0],
        dps: DPS,
        band: BAND,
        lit: false,
    }
}

// ---------------------------------------------------------------------------------------------------------
// The row's acceptance test: line of sight goes through the trait, and an open mouth is seen through.
// ---------------------------------------------------------------------------------------------------------

/// A cliff face across z = 0 with a tunnel mouth in it: 20 m wide (x -10..10), 12 m high (y 0..12). An
/// observer stands 50 m out on the open side; enemies wait 60 m inside the rock. Only the enemy the mouth
/// frames is seen; closing the mouth (a blocker) hides it too. The board's radar is short (40 m) so every
/// sighting here is a direct line of sight.
#[test]
fn los_trait_sees_through_an_open_mouth() {
    let mouth = MouthWall::new(0.0, [-10.0, 10.0], [0.0, 12.0]);
    let eye = [0.0, 2.0, -50.0];
    let in_the_tube = [0.0, 2.0, 60.0];
    let in_the_rock = [40.0, 2.0, 60.0];

    // The trait itself.
    assert!(mouth.clear(eye, in_the_tube), "the open mouth frames the tube");
    assert!(!mouth.clear(eye, in_the_rock), "the cliff face hides the rock");
    assert!(!mouth.closed().clear(eye, in_the_tube), "a closed mouth blocks");
    assert!(
        mouth.clear(eye, [30.0, 2.0, -10.0]),
        "both ends on the open side never cross the face"
    );
    assert!(OpenField.clear(eye, in_the_rock), "open field sees everything");
    assert!(
        None::<&MouthWall>.clear(eye, in_the_rock),
        "no geometry is open field"
    );
    assert!(
        !Some(&mouth).clear(eye, in_the_rock),
        "Some(geometry) is that geometry"
    );

    // Through the sighting board.
    let observer = PerceptUnit {
        id: 1,
        team: 1,
        pos: eye,
        sight_range_m: SIGHT_M,
        radar_scale: 1.0,
    };
    let enemy = |id: u32, pos: [f32; 3]| PerceptEnemy {
        pos,
        ..foe(id, 2, 0.0)
    };
    let enemies = [enemy(10, in_the_tube), enemy(11, in_the_rock)];
    let mut board = SightingsBoard::new(3, 40.0);
    board.refresh(mouth, 0, &[observer], &enemies, &params());
    let row = board.for_team(1);
    assert_eq!(
        row.len(),
        1,
        "only the enemy in the mouth's frame is seen: {row:?}"
    );
    assert_eq!(row[0].entity, 10);
    assert_eq!(row[0].sighting.source, SightSource::DirectLos);

    let mut shut = SightingsBoard::new(3, 40.0);
    shut.refresh(mouth.closed(), 0, &[observer], &enemies, &params());
    assert!(shut.for_team(1).is_empty(), "a closed mouth hides the tube");

    // The same board with no geometry (DO27's `None` terrain) sees both.
    let mut open = SightingsBoard::new(3, 40.0);
    open.refresh(OpenField, 0, &[observer], &enemies, &params());
    assert_eq!(open.for_team(1).len(), 2);
}

/// A host query plugs in as a closure: the board asks it once per observer and enemy pair it re-checks, and
/// never for a pair out of sight range.
#[test]
fn los_fn_plugs_a_host_query_in_and_is_asked_only_in_sight_range() {
    use std::cell::Cell;
    let asked = Cell::new(0u32);
    let los = LosFn(|_from: [f32; 3], _to: [f32; 3]| {
        asked.set(asked.get() + 1);
        false
    });
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let units = [observer(1, 1, 0.0)];
    // 100 m: in sight range, LOS asked (and refused), then radar (400 m) still lists it.
    // 5000 m: out of sight range, LOS never asked.
    let enemies = [foe(11, 2, 100.0), foe(12, 2, 5000.0)];
    board.refresh(los, 0, &units, &enemies, &params());
    assert_eq!(asked.get(), 1);
    let row = board.for_team(1);
    assert_eq!(row.len(), 1);
    assert_eq!(
        row[0].sighting.source,
        SightSource::Radar,
        "LOS refused, radar still sees it"
    );
}

// ---------------------------------------------------------------------------------------------------------
// DO27's board tests (fog_of_war.rs, satellite_fog.rs), assertions unchanged.
// ---------------------------------------------------------------------------------------------------------

#[test]
fn team_vision_is_the_union_of_its_members() {
    let t = flat();
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let units = [
        observer(1, 1, 0.0),
        observer(2, 1, 1000.0),
        observer(3, 2, 5000.0),
    ];
    // Each enemy is in range of exactly one team-1 observer (900 m clears
    // both the 220 m sight and the 400 m radar of the other).
    let enemies = [foe(11, 2, 100.0), foe(12, 2, 900.0)];
    board.refresh(Some(&t), 0, &units, &enemies, &params());
    let seen: Vec<u32> = board.for_team(1).iter().map(|k| k.entity).collect();
    assert_eq!(seen, vec![11, 12], "both enemies on the team-1 row");
    assert!(board.for_team(2).is_empty(), "team 2 lists no enemy");
    assert!(board.for_team(3).is_empty(), "team 3 lists no enemy");
}

/// Shared radar needs no direct line of sight: inside team radar range but
/// past the observer's own sight range, the contact still lands on the row.
#[test]
fn radar_contact_is_shared_without_direct_los() {
    let t = flat();
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let units = [PerceptUnit {
        id: 1,
        team: 1,
        pos: [0.0, 0.0, 0.0],
        sight_range_m: 50.0,
        radar_scale: 1.0,
    }];
    // 300 m: past sight (50 m), inside team radar (400 m).
    let enemies = [foe(11, 2, 300.0)];
    board.refresh(Some(&t), 0, &units, &enemies, &params());
    let row = board.for_team(1);
    assert_eq!(row.len(), 1, "radar contact is on the team row");
    assert_eq!(row[0].sighting.source, SightSource::Radar);
}

/// An enemy outside every teammate's sight and radar stays out of the team
/// picture, and a team never lists its own units as enemies.
#[test]
fn enemy_outside_every_teammate_range_stays_off_the_team_picture() {
    let t = flat();
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let units = [observer(1, 1, 0.0), observer(2, 1, 200.0)];
    let enemies = [foe(11, 2, 5000.0)];
    board.refresh(Some(&t), 0, &units, &enemies, &params());
    assert!(board.for_team(1).is_empty(), "far enemy stays hidden from team 1");
    assert!(board.for_team(2).is_empty(), "team 2 never lists its own unit");
}

/// One team's sightings never leak onto another team's picture: the enemy is
/// on team 2's row only, so a live team-1 member never sees team-2-only fog.
#[test]
fn one_team_sightings_never_leak_onto_another_team_picture() {
    let t = flat();
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let units = [observer(1, 2, 0.0), observer(2, 1, 5000.0)];
    let enemies = [foe(11, 1, 100.0)];
    board.refresh(Some(&t), 0, &units, &enemies, &params());
    assert_eq!(board.for_team(2).len(), 1, "team 2 spots the enemy");
    assert!(board.for_team(1).is_empty(), "team 1 sees no enemy fog");
}

/// Fog decay: an unseen sighting loses confidence and is forgotten, while a
/// re-spotted enemy returns at full confidence where newly seen.
#[test]
fn unseen_sightings_decay_and_are_forgotten_until_respotted() {
    let t = flat();
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let units = [observer(1, 1, 0.0)];
    board.refresh(Some(&t), 0, &units, &[foe(11, 2, 100.0)], &params());
    assert_eq!(board.for_team(1).len(), 1, "contact spotted");
    // Out of contact the entry lingers but weakens: decay, not an instant drop.
    let empty: [PerceptEnemy; 0] = [];
    for tick in 1..=3u64 {
        board.refresh(Some(&t), tick, &units, &empty, &params());
    }
    let row = board.for_team(1);
    assert_eq!(row.len(), 1, "recent contact is still listed");
    assert!(
        row[0].sighting.confidence < 1.0,
        "unseen confidence decays, got {}",
        row[0].sighting.confidence
    );
    // Left unseen long enough it is forgotten entirely.
    for tick in 4..=80u64 {
        board.refresh(Some(&t), tick, &units, &empty, &params());
    }
    assert!(board.for_team(1).is_empty(), "long-unseen contact is forgotten");
    // A fresh sighting returns at full confidence where newly seen.
    let back = [PerceptEnemy {
        pos: [150.0, 0.0, 0.0],
        ..foe(11, 2, 150.0)
    }];
    board.refresh(Some(&t), 81, &units, &back, &params());
    let row = board.for_team(1);
    assert_eq!(row.len(), 1, "re-spotted contact returns");
    assert_eq!(row[0].sighting.confidence, 1.0);
    assert_eq!(row[0].sighting.last_pos, [150.0, 0.0, 0.0]);
}

/// The board half of DO27's `pipeline_is_deterministic_over_ticks`: two boards fed the same moving world
/// stay equal every tick (LOS re-checks on the default 12-tick phase), and team 1 ends with an enemy that
/// can reach it.
#[test]
fn board_refresh_is_deterministic_over_ticks() {
    let ridge = HeightfieldLos(|_x: f32, z: f32| if (z - 160.0).abs() < 60.0 { 20.0 } else { 0.0 });
    let params = vec![PerceptionParams::default(); 3];
    let mut a = SightingsBoard::new(3, RADAR_RANGE_M);
    let mut b = SightingsBoard::new(3, RADAR_RANGE_M);
    for tick in 0..120u64 {
        let t = tick as f32;
        let units: Vec<PerceptUnit> = (0..6u32)
            .map(|i| PerceptUnit {
                id: i + 1,
                team: 1 + (i % 2) as u8,
                pos: [
                    i as f32 * 30.0 - 75.0,
                    0.0,
                    if i % 2 == 0 { -50.0 } else { 300.0 } - t,
                ],
                sight_range_m: SIGHT_M,
                radar_scale: 1.0,
            })
            .collect();
        let enemies: Vec<PerceptEnemy> = units
            .iter()
            .map(|u| PerceptEnemy {
                id: u.id,
                team: u.team,
                pos: u.pos,
                vel: [0.0, -60.0],
                dps: DPS,
                band: BAND,
                lit: tick % 30 == 0,
            })
            .collect();
        a.refresh(ridge, tick, &units, &enemies, &params);
        b.refresh(ridge, tick, &units, &enemies, &params);
        assert_eq!(a, b, "board drifted at tick {tick}");
    }
    assert!(a.for_team(1).iter().any(|k| k.threat.can_reach_me));
}

// ---------------------------------------------------------------------------------------------------------
// Cross-arch bit pins.
// ---------------------------------------------------------------------------------------------------------

fn fnv(words: impl IntoIterator<Item = u64>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// 3000 ticks of the grid: six threat stamps and four control stamps circling (lockstep trig), one stamp and
/// one decay per tick; then the cells, a force-ratio sweep and flank bearings behind a ridge, as bits.
fn influence_bits() -> u64 {
    use regia_detsim::math;
    let ridge = HeightfieldLos(|_x: f32, z: f32| if (z - 60.0).abs() < 20.0 { 15.0 } else { 0.0 });
    let mut grid = InfluenceGrid::new([-300.0, -300.0], [300.0, 300.0], INFLUENCE_CELL_M);
    let mut words = Vec::new();
    for tick in 0..3000u32 {
        let t = tick as f32 * 0.01;
        let threat: Vec<([f32; 2], f32)> = (0..6)
            .map(|i| {
                let a = t + i as f32;
                ([math::cos(a) * 120.0, math::sin(a) * 90.0], 5.0 + i as f32)
            })
            .collect();
        let control: Vec<([f32; 2], f32)> = (0..4)
            .map(|i| {
                let a = -t * 0.5 + i as f32 * 1.7;
                ([math::sin(a) * 200.0, math::cos(a) * 60.0 - 40.0], 8.0)
            })
            .collect();
        grid.set_stamps(&threat, &control);
        grid.stamp_one();
        grid.decay_one();
        if tick % 250 == 0 {
            words.push(u64::from(grid.local_force_ratio(0.0, 0.0).to_bits()));
        }
    }
    words.extend(grid.threat_cells().iter().map(|v| u64::from(v.to_bits())));
    words.extend(grid.control_cells().iter().map(|v| u64::from(v.to_bits())));
    for i in -20..=20 {
        let x = i as f32 * 13.5;
        words.push(u64::from(grid.local_force_ratio(x, -x * 0.5).to_bits()));
        let unit = [x, 0.0, 0.0];
        let threat = [x * 0.3, 0.0, 140.0];
        words.push(u64::from(flank_bearing(ridge, unit, threat, x * 0.1).to_bits()));
        let friendly = [[x * 0.1, 0.0, 30.0], [-x, 0.0, 10.0]];
        words.push(u64::from(line_of_fire(unit, threat, &friendly)));
    }
    fnv(words)
}

/// 600 ticks of a 3-team board over a ridge and a mouth wall (team 2 walks from behind the face, through the
/// mouth plane and over the ridge toward team 1), with
/// the default 12-tick LOS phase, lit pulses and a departure; every row's fields as bits.
fn perception_bits() -> u64 {
    let ridge = HeightfieldLos(|_x: f32, z: f32| if (z - 160.0).abs() < 30.0 { 25.0 } else { 0.0 });
    let mouth = MouthWall::new(250.0, [-12.0, 12.0], [-1.0, 10.0]);
    let world = LosFn(|a: [f32; 3], b: [f32; 3]| ridge.clear(a, b) && mouth.clear(a, b));
    let mut board = SightingsBoard::new(3, 180.0).with_lit_bonus_m(90.0);
    let params = [
        PerceptionParams::default(),
        PerceptionParams::default(),
        PerceptionParams {
            los_recheck_interval_ticks: 5,
            confidence_decay: 0.9,
            ..PerceptionParams::default()
        },
    ];
    let mut words = Vec::new();
    // What the scenario exercises: (direct rows, radar rows, rows forgotten).
    let mut seen = (0u32, 0u32, 0u32);
    for tick in 0..600u64 {
        let t = tick as f32;
        let units: Vec<PerceptUnit> = (0..8u32)
            .map(|i| PerceptUnit {
                id: i * 3 + 1,
                team: 1 + (i % 2) as u8,
                pos: [
                    i as f32 * 20.0 - 70.0,
                    1.5,
                    if i % 2 == 0 { -t * 0.2 } else { 320.0 - t * 0.5 },
                ],
                sight_range_m: 260.0,
                radar_scale: if i == 5 { 0.5 } else { 1.0 },
            })
            .collect();
        let enemies: Vec<PerceptEnemy> = units
            .iter()
            .filter(|u| !(u.id == 4 && tick > 400))
            .map(|u| PerceptEnemy {
                id: u.id,
                team: u.team,
                pos: [u.pos[0] * 0.2, u.pos[1], u.pos[2] + (t * 0.7) % 5.0],
                vel: [0.0, -0.2],
                dps: 40.0 + u.id as f32,
                band: [0.0, 120.0 + u.id as f32],
                lit: (tick + u64::from(u.id)) % 45 == 0,
            })
            .collect();
        let before: usize = board.by_team.iter().map(Vec::len).sum();
        board.refresh(world, tick, &units, &enemies, &params);
        let after: usize = board.by_team.iter().map(Vec::len).sum();
        if after < before {
            seen.2 += 1;
        }
        for team in 0..3u8 {
            for k in board.for_team(team) {
                let s = &k.sighting;
                words.push(u64::from(k.entity) | (u64::from(team) << 32));
                words.extend(s.last_pos.iter().map(|v| u64::from(v.to_bits())));
                words.push(u64::from(s.confidence.to_bits()));
                words.push(s.last_seen_tick ^ (s.first_seen_tick << 20));
                words.push(s.source as u64);
                match s.source {
                    SightSource::DirectLos => seen.0 += 1,
                    _ => seen.1 += 1,
                }
                words.push(u64::from(k.threat.can_reach_me) | (u64::from(k.threat.incoming) << 1));
            }
            words.push(u64::from(board.visible_threat(team).to_bits()));
        }
    }
    assert!(
        seen.0 > 0 && seen.1 > 0 && seen.2 > 0,
        "the pinned scenario must see directly, by radar and forget: {seen:?}"
    );
    fnv(words)
}

#[test]
fn influence_and_perception_bits_match_the_pinned_hashes() {
    // The pins are the same on x86-64 and aarch64 (the cross-arch proof runs this test there too: the Mac,
    // the Android test device and qemu-user). A change here is a sim-version change (ADR-8012 A-4).
    let got = [influence_bits(), perception_bits()];
    eprintln!(
        "perception bits: influence {:#018x} board {:#018x}",
        got[0], got[1]
    );
    assert_eq!(got, PINNED, "influence or perception bits moved");
}

/// Pinned on x86-64 Linux (the test machine); see `influence_and_perception_bits_match_the_pinned_hashes`.
/// The influence hash is re-pinned for ADR-8876 (`MATH_BITS_VERSION` 1, the canonical `math::atan2`; was
/// `0x4816_a5f1_99e3_3724`); the perception hash did not move.
const PINNED: [u64; 2] = [0x77bd_d039_d475_4bf7, 0xc3d7_4ea6_59ba_aac7];
