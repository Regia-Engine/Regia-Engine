// SPDX-License-Identifier: MPL-2.0
//! Terrain line of sight for fog of war (plan ai-nav-18): `GridLineOfSight` over the det grid's
//! heights plugged into `regia_detsim::fog`.

use std::sync::Arc;

use regia_detsim::fog::{
    FogEntity, FogGrid, FogRules, FogWorld, SightClass, TeamId, TeamVision, entity_visible_to_team,
};
use regia_detsim::los::{LineOfSight, OpenGround};
use regia_nav::det::fog_los::GridLineOfSight;
use regia_nav::det::grid::{GridSettings, HeightCells, NavStatic};

/// 5 m cells, 96 x 32 samples (480 m x 160 m, centered): a 30 m hill whose crest runs along
/// x = 0 (a smooth ridge, 25 m to each side), flat ground elsewhere.
fn hill() -> Arc<NavStatic> {
    let (cols, rows, cell) = (96usize, 32usize, 5.0f32);
    let mut heights = Vec::with_capacity(cols * rows);
    for _z in 0..rows {
        for x in 0..cols {
            let wx = x as f32 * cell - cols as f32 * cell * 0.5;
            let d = if wx < 0.0 { -wx } else { wx };
            heights.push(if d < 25.0 { 30.0 * (1.0 - d / 25.0) } else { 0.0 });
        }
    }
    let ground = HeightCells::centered(cols, rows, cell, heights);
    match NavStatic::build(Some(&ground), Arc::new(GridSettings::hover_ground_jet())) {
        Ok(stat) => Arc::new(stat),
        Err(err) => panic!("hill fixture: {err:?}"),
    }
}

/// Rows of the test: (id, team, class, position).
struct Field(Vec<(u64, TeamId, SightClass, [f32; 3])>);

impl FogWorld for Field {
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
        self.0
            .iter()
            .map(|&(id, team, class, at)| FogEntity::new(id, "scout", class, team, at))
    }
}

fn by_id(field: &Field, id: u64) -> FogEntity<'_> {
    match field.fog_entities().find(|e| e.id == id) {
        Some(e) => e,
        None => panic!("no entity {id}"),
    }
}

#[test]
fn a_hill_blocks_sight_and_the_top_sees_over_it() {
    let stat = hill();
    let los = GridLineOfSight::new(stat).with_eye_m(1.8).with_target_m(1.0);
    assert_eq!(los.ground_at(0.0, 0.0), 30.0);
    assert_eq!(los.ground_at(-100.0, 0.0), 0.0);
    // Valley to valley across the crest, 120 m apart (inside the 150 m unit sight): blocked.
    assert!(!los.clear([-60.0, 0.0, 0.0], [60.0, 0.0, 0.0]));
    assert!(!los.clear([60.0, 0.0, 0.0], [-60.0, 0.0, 0.0]), "symmetric");
    // Along the valley floor, no hill between: clear.
    assert!(los.clear([-60.0, 0.0, -50.0], [-60.0, 0.0, 50.0]));
    // From the crest, both valleys show.
    assert!(los.clear([0.0, 30.0, 0.0], [60.0, 0.0, 0.0]));
    assert!(los.clear([0.0, 30.0, 0.0], [-60.0, 0.0, 0.0]));
    // A point below the ground (a fog cell's query height) is lifted onto it.
    assert!(los.clear([0.0, 30.0, 0.0], [60.0, f32::MIN, 0.0]));
    // An eye high enough over the valley clears the crest.
    assert!(los.clear([-60.0, 80.0, 0.0], [60.0, 0.0, 0.0]));

    // Through fog: team 1's scout in the west valley, team 2's in the east one.
    let mut field = Field(vec![
        (1, 1, SightClass::Unit, [-60.0, 0.0, 0.0]),
        (2, 2, SightClass::Unit, [60.0, 0.0, 0.0]),
    ]);
    let rules = FogRules::default();
    assert!(entity_visible_to_team(
        &field,
        &rules,
        1,
        &by_id(&field, 2),
        &OpenGround
    ));
    assert!(!entity_visible_to_team(
        &field,
        &rules,
        1,
        &by_id(&field, 2),
        &los
    ));
    let vision = TeamVision::new(&field, &rules, 1).with_index_min_watchers(0);
    assert!(!vision.entity_visible(&field, &by_id(&field, 2), &los));
    // A team 1 scout on the hilltop sees over it.
    field.0.push((3, 1, SightClass::Unit, [0.0, 30.0, 0.0]));
    assert!(entity_visible_to_team(&field, &rules, 1, &by_id(&field, 2), &los));
    let mut vision = TeamVision::default().with_index_min_watchers(0);
    vision.rebuild(&field, &rules, 1);
    assert!(vision.is_indexed());
    assert!(vision.entity_visible(&field, &by_id(&field, 2), &los));
    // A tall watchtower eye in the valley sees over it too (a per-class eye height).
    field.0.truncate(2);
    field.0[0].2 = SightClass::Building;
    let mut tall = FogRules::default();
    tall.sight.building_m = 150.0;
    tall.sight.building_eye_m = 80.0;
    assert!(entity_visible_to_team(&field, &tall, 1, &by_id(&field, 2), &los));
}

#[test]
fn the_fog_grid_lights_only_the_near_side_of_the_hill() {
    let los = GridLineOfSight::new(hill()).with_eye_m(1.8);
    let field = Field(vec![(1, 1, SightClass::Unit, [-60.0, 0.0, 0.0])]);
    let vision = TeamVision::new(&field, &FogRules::default(), 1);
    let mut grid = FogGrid::covering(240.0, 80.0);
    let mut open = grid.clone();
    grid.update(&vision, &los);
    open.update(&vision, &OpenGround);
    assert!(grid.visible_at(-60.0, 0.0));
    assert!(grid.visible_at(-20.0, 0.0), "the near slope");
    assert!(!grid.visible_at(60.0, 0.0), "the far valley");
    assert!(open.visible_at(60.0, 0.0));
    assert!(grid.visible_count() < open.visible_count());
}

#[test]
fn grid_line_of_sight_is_the_same_on_every_call_and_order() {
    let los = GridLineOfSight::new(hill());
    let pts = [
        [-200.0, 0.0, -70.0],
        [-60.0, 2.0, 10.0],
        [0.0, 30.0, 0.0],
        [10.0, 0.0, 40.0],
        [190.0, 0.0, 75.0],
        [f32::NAN, 0.0, 0.0],
        [1.0e9, 0.0, -1.0e9],
    ];
    let mut first = Vec::new();
    for a in pts {
        for b in pts {
            first.push(los.clear(a, b));
        }
    }
    let mut again = Vec::new();
    for a in pts.iter().rev() {
        for b in pts.iter().rev() {
            again.push(los.clear(*a, *b));
        }
    }
    again.reverse();
    assert_eq!(first, again);
    // The same cell is always clear.
    assert!(los.clear([3.0, 0.0, 3.0], [4.0, 0.0, 4.0]));
}
