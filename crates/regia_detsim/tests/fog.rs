// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/fog.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (plan row S-16, ADR-8154). Lines 217-516 of the source: its eight tests, ported. The
// game's state struct and spawn helper became the small `Field` world below (the `FogWorld` seam);
// game content kind names became neutral ones; the radar structures are kind rows of the test's
// sight table (the sight table is data now). Every assertion is the source's. The tests after the
// eight are new (Regia): the seams, the settings and the equivalences the adaptation relies on.

//! Fog of war: DO27's eight tests ported, plus the adaptation's own (plan row S-16).

use regia_detsim::fog::{
    DEFAULT_BUILDING_SIGHT_M, DEFAULT_ON_FOOT_SIGHT_M, DEFAULT_UNIT_SIGHT_M, FOG_GRID_MAX_CELLS, FogEntity,
    FogGrid, FogGridError, FogRules, FogWorld, SightClass, SightTable, TeamId, TeamVision,
    entity_visible_to_team, team_sees_point, visible_ids,
};
use regia_detsim::los::{LineOfSight, OpenGround};

/// The long sight radius the tests give radar structures (a kind row, not an engine constant).
const RADAR_SIGHT_M: f32 = 400.0;

/// One entity of the test world.
#[derive(Debug, Clone)]
struct Row {
    id: u64,
    kind: String,
    class: SightClass,
    team: TeamId,
    position: [f32; 3],
    alive: bool,
}

/// A tiny sim state: entities in spawn order and a symmetric alliance list.
#[derive(Debug, Clone, Default)]
struct Field {
    rows: Vec<Row>,
    allies: Vec<(TeamId, TeamId)>,
    fog_enabled: bool,
}

impl Field {
    fn new() -> Self {
        Self {
            fog_enabled: true,
            ..Self::default()
        }
    }

    /// Spawns a living entity (DO27's `stamp` helper).
    fn stamp(&mut self, id: u64, class: SightClass, team: TeamId, kind: &str, position: [f32; 3]) -> u64 {
        self.rows.push(Row {
            id,
            kind: kind.to_string(),
            class,
            team,
            position,
            alive: true,
        });
        id
    }

    fn row_mut(&mut self, id: u64) -> &mut Row {
        match self.rows.iter_mut().find(|row| row.id == id) {
            Some(row) => row,
            None => panic!("no entity {id}"),
        }
    }

    fn by_id(&self, id: u64) -> FogEntity<'_> {
        match self.fog_entities().find(|e| e.id == id) {
            Some(e) => e,
            None => panic!("no entity {id}"),
        }
    }

    fn set_allied(&mut self, a: TeamId, b: TeamId, allied: bool) {
        let pair = (a.min(b), a.max(b));
        self.allies.retain(|&p| p != pair);
        if allied {
            self.allies.push(pair);
        }
    }

    /// The match's fog rules: the defaults, the radar kinds, and the fog switch.
    fn rules(&self) -> FogRules {
        FogRules {
            enabled: self.fog_enabled,
            sight: SightTable::default()
                .with_kind("radar_mast", RADAR_SIGHT_M)
                .with_kind("beacon", RADAR_SIGHT_M),
            ..FogRules::default()
        }
    }

    fn visible(&self, viewer: TeamId, id: u64) -> bool {
        entity_visible_to_team(self, &self.rules(), viewer, &self.by_id(id), &OpenGround)
    }
}

impl FogWorld for Field {
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
        self.rows.iter().map(|row| FogEntity {
            id: row.id,
            kind: &row.kind,
            class: row.class,
            team: row.team,
            position: row.position,
            alive: row.alive,
            kind_id: None,
        })
    }

    fn is_allied(&self, a: TeamId, b: TeamId) -> bool {
        self.allies.contains(&(a.min(b), a.max(b)))
    }
}

// ---------------------------------------------------------------------------------------------
// DO27's eight tests, ported.
// ---------------------------------------------------------------------------------------------

#[test]
fn sight_table_covers_units_buildings_radar_and_pilots() {
    let table = Field::new().rules().sight;
    assert_eq!(table.radius_m("scout", SightClass::Unit), DEFAULT_UNIT_SIGHT_M);
    assert_eq!(
        table.radius_m("watchtower", SightClass::Building),
        DEFAULT_BUILDING_SIGHT_M
    );
    assert_eq!(table.radius_m("radar_mast", SightClass::Building), RADAR_SIGHT_M);
    assert_eq!(table.radius_m("beacon", SightClass::Building), RADAR_SIGHT_M);
    // A crew member out of its vehicle: the on-foot class, whatever its kind.
    assert_eq!(
        table.radius_m("hull", SightClass::OnFoot),
        DEFAULT_ON_FOOT_SIGHT_M
    );
    // The crew kind itself can carry the on-foot radius as a kind row.
    let with_crew = table.clone().with_kind("crew", DEFAULT_ON_FOOT_SIGHT_M);
    assert_eq!(
        with_crew.radius_m("crew", SightClass::Unit),
        DEFAULT_ON_FOOT_SIGHT_M
    );
    assert_eq!(table.radius_m("vent", SightClass::Feature), 0.0);
}

#[test]
fn a_unit_outside_every_friendly_sight_radius_is_hidden() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(
        202,
        SightClass::Unit,
        2,
        "scout",
        [DEFAULT_UNIT_SIGHT_M + 50.0, 0.0, 0.0],
    );
    assert!(!state.visible(1, 202));
}

#[test]
fn it_appears_when_a_friendly_unit_comes_in_range() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(
        202,
        SightClass::Unit,
        2,
        "scout",
        [DEFAULT_UNIT_SIGHT_M + 50.0, 0.0, 0.0],
    );
    assert!(!state.visible(1, 202));
    state.row_mut(101).position = [DEFAULT_UNIT_SIGHT_M, 0.0, 0.0];
    assert!(state.visible(1, 202));
}

#[test]
fn fog_off_shows_everything() {
    let mut state = Field::new();
    state.fog_enabled = false;
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(202, SightClass::Unit, 2, "scout", [5000.0, 0.0, 5000.0]);
    assert!(state.visible(1, 202));
}

#[test]
fn both_teams_views_differ_correctly() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(202, SightClass::Unit, 2, "scout", [100.0, 0.0, 0.0]);
    state.stamp(203, SightClass::Unit, 2, "scout", [5000.0, 0.0, 0.0]);
    // Team 1's watcher at the origin sees team 2's near unit, not its far one.
    assert!(state.visible(1, 202));
    assert!(!state.visible(1, 203));
    // Team 2 sees its own far unit, but not a team 1 unit far from every team 2 watcher.
    state.stamp(102, SightClass::Unit, 1, "scout", [-5000.0, 0.0, 0.0]);
    assert!(state.visible(2, 203));
    assert!(!state.visible(2, 102));
    assert!(!state.visible(1, 203));
}

#[test]
fn own_allied_and_neutral_entities_are_always_visible() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(301, SightClass::Unit, 3, "scout", [5000.0, 0.0, 0.0]);
    state.stamp(401, SightClass::Feature, 0, "vent", [5000.0, 0.0, 5000.0]);
    state.set_allied(1, 3, true);
    assert!(state.visible(1, 101));
    assert!(state.visible(1, 301));
    assert!(state.visible(1, 401));
}

#[test]
fn radar_structures_watch_far_and_the_dead_watch_nothing() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Building, 1, "radar_mast", [0.0, 0.0, 0.0]);
    state.stamp(
        202,
        SightClass::Unit,
        2,
        "scout",
        [RADAR_SIGHT_M - 10.0, 0.0, 0.0],
    );
    assert!(state.visible(1, 202));
    state.row_mut(101).alive = false;
    assert!(!state.visible(1, 202));
}

#[test]
fn fog_grid_covers_1280_exactly_and_2050_past_the_rim() {
    let alpha = FogGrid::covering(640.0, 640.0);
    assert_eq!(alpha.cols, 40);
    assert_eq!(alpha.rows, 40);
    assert_eq!(alpha.origin, [-640.0, -640.0]);
    assert!(alpha.contains(0.0, 0.0));
    assert!(!alpha.contains(900.0, 0.0));
    let wash = FogGrid::covering(1025.0, 1025.0);
    assert_eq!(wash.cols, 65);
    assert_eq!(wash.rows, 65);
    assert_eq!(wash.origin[0], -1040.0);
    assert_eq!(wash.origin[1], -1040.0);
    assert!(wash.contains(900.0, 0.0));
    assert!(!wash.contains(1100.0, 0.0));
    let mut stamped = FogGrid::covering(1025.0, 1025.0);
    stamped.stamp(0.0, 0.0, DEFAULT_UNIT_SIGHT_M);
    assert!(stamped.seen_at(100.0, 0.0));
    assert!(!stamped.seen_at(900.0, 0.0));
    assert_eq!(DEFAULT_UNIT_SIGHT_M, 150.0);
    assert_eq!(DEFAULT_BUILDING_SIGHT_M, 100.0);
    let fallback = FogGrid::covering(f32::NAN, 0.0);
    assert_eq!(fallback.cols, 40);
    assert_eq!(fallback.origin[0], -640.0);
}

// ---------------------------------------------------------------------------------------------
// Regia's tests for the adaptation.
// ---------------------------------------------------------------------------------------------

/// A small deterministic generator (SplitMix64) for the equivalence sweeps.
struct Mix(u64);

impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[lo, hi)`.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        lo + (hi - lo) * unit
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A random field: `n` entities of every class over teams 0..=5 inside +-600 m, some dead, with
/// teams 1 and 3 allied.
fn random_field(seed: u64, n: u64) -> Field {
    let mut mix = Mix(seed);
    let mut field = Field::new();
    let kinds = ["scout", "radar_mast", "beacon", "watchtower", "crew"];
    for id in 0..n {
        let class = match mix.below(4) {
            0 => SightClass::Unit,
            1 => SightClass::OnFoot,
            2 => SightClass::Building,
            _ => SightClass::Feature,
        };
        let kind = kinds[mix.below(kinds.len() as u64) as usize];
        let team = mix.below(6) as TeamId;
        let position = [
            mix.range(-600.0, 600.0),
            mix.range(-5.0, 5.0),
            mix.range(-600.0, 600.0),
        ];
        field.stamp(id + 1, class, team, kind, position);
        if mix.below(8) == 0 {
            field.row_mut(id + 1).alive = false;
        }
    }
    field.set_allied(1, 3, true);
    field
}

#[test]
fn team_vision_answers_the_same_as_the_free_functions() {
    let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 37.5) == (to[0] < 37.5);
    for seed in 0..24u64 {
        let field = random_field(seed, 60);
        let rules = field.rules();
        for viewer in 0..=6 {
            let vision = TeamVision::new(&field, &rules, viewer);
            for entity in field.fog_entities() {
                assert_eq!(
                    vision.entity_visible(&field, &entity, &OpenGround),
                    entity_visible_to_team(&field, &rules, viewer, &entity, &OpenGround),
                    "seed {seed} viewer {viewer} entity {}",
                    entity.id
                );
                assert_eq!(
                    vision.entity_visible(&field, &entity, &wall),
                    entity_visible_to_team(&field, &rules, viewer, &entity, &wall),
                    "seed {seed} viewer {viewer} entity {} (wall)",
                    entity.id
                );
            }
            let mut mix = Mix(seed ^ 0xF06);
            for _ in 0..40 {
                let p = [mix.range(-700.0, 700.0), 0.0, mix.range(-700.0, 700.0)];
                assert_eq!(
                    vision.sees_point(p, &OpenGround),
                    team_sees_point(&field, &rules, viewer, p, &OpenGround)
                );
            }
        }
    }
}

#[test]
fn a_wall_hides_a_unit_inside_sight_radius() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(202, SightClass::Unit, 2, "scout", [100.0, 0.0, 0.0]);
    let rules = state.rules();
    // A wall on the plane x = 50 between them.
    let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 50.0) == (to[0] < 50.0);
    let enemy = state.by_id(202);
    assert!(entity_visible_to_team(&state, &rules, 1, &enemy, &OpenGround));
    assert!(!entity_visible_to_team(&state, &rules, 1, &enemy, &wall));
    // A second watcher on the far side of the wall sees it again.
    state.stamp(102, SightClass::Unit, 1, "scout", [60.0, 0.0, 30.0]);
    let enemy = state.by_id(202);
    assert!(entity_visible_to_team(&state, &rules, 1, &enemy, &wall));
}

#[test]
fn the_line_of_sight_hook_is_asked_only_for_watchers_in_range() {
    use std::cell::Cell;
    struct Counting(Cell<u32>);
    impl LineOfSight for Counting {
        fn clear(&self, _from: [f32; 3], _to: [f32; 3]) -> bool {
            self.0.set(self.0.get() + 1);
            false
        }
    }
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(102, SightClass::Unit, 1, "scout", [3000.0, 0.0, 0.0]);
    state.stamp(202, SightClass::Unit, 2, "scout", [100.0, 0.0, 0.0]);
    let los = Counting(Cell::new(0));
    assert!(!entity_visible_to_team(
        &state,
        &state.rules(),
        1,
        &state.by_id(202),
        &los
    ));
    assert_eq!(los.0.get(), 1, "only the watcher in range casts a ray");
}

#[test]
fn team_count_is_a_setting() {
    let mut state = Field::new();
    state.stamp(501, SightClass::Unit, 5, "scout", [0.0, 0.0, 0.0]);
    state.stamp(601, SightClass::Unit, 6, "scout", [5000.0, 0.0, 0.0]);
    let four = state.rules();
    // With four teams, team 5 is neutral (always shown) and as a viewer an observer.
    assert!(entity_visible_to_team(
        &state,
        &four,
        1,
        &state.by_id(501),
        &OpenGround
    ));
    assert!(entity_visible_to_team(
        &state,
        &four,
        5,
        &state.by_id(601),
        &OpenGround
    ));
    let six = FogRules {
        team_count: 6,
        ..state.rules()
    };
    // With six, team 5 plays: its far enemy is hidden from it, and it hides from team 1.
    assert!(!entity_visible_to_team(
        &state,
        &six,
        5,
        &state.by_id(601),
        &OpenGround
    ));
    assert!(!entity_visible_to_team(
        &state,
        &six,
        1,
        &state.by_id(501),
        &OpenGround
    ));
}

#[test]
fn kind_rows_override_and_a_zero_row_never_watches() {
    let table = SightTable::default()
        .with_kind("decoy", 0.0)
        .with_kind("decoy", 0.0);
    assert_eq!(table.kinds.len(), 1, "with_kind replaces a row");
    assert_eq!(table.radius_m("decoy", SightClass::Unit), 0.0);
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "decoy", [0.0, 0.0, 0.0]);
    state.stamp(202, SightClass::Unit, 2, "scout", [10.0, 0.0, 0.0]);
    let rules = FogRules {
        sight: table,
        ..FogRules::default()
    };
    assert!(!entity_visible_to_team(
        &state,
        &rules,
        1,
        &state.by_id(202),
        &OpenGround
    ));
}

#[test]
fn an_invalid_radius_is_named() {
    assert_eq!(SightTable::default().first_invalid(), None);
    let bad = SightTable {
        on_foot_m: f32::NAN,
        ..SightTable::default()
    };
    assert_eq!(
        bad.first_invalid().map(|(what, _)| what),
        Some("on_foot_m".to_string())
    );
    let bad_kind = SightTable::default().with_kind("mast", -1.0);
    assert_eq!(bad_kind.first_invalid(), Some(("kinds[mast]".to_string(), -1.0)));
}

#[test]
fn visible_ids_are_sorted_and_count_the_hidden() {
    let mut state = Field::new();
    state.stamp(9, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(3, SightClass::Unit, 2, "scout", [100.0, 0.0, 0.0]);
    state.stamp(7, SightClass::Unit, 2, "scout", [4000.0, 0.0, 0.0]);
    state.stamp(1, SightClass::Feature, 0, "vent", [4000.0, 0.0, 4000.0]);
    let mut vision = TeamVision::default();
    let mut ids = vec![42];
    let hidden = visible_ids(&state, &state.rules(), 1, &OpenGround, &mut vision, &mut ids);
    assert_eq!(ids, vec![1, 3, 9]);
    assert_eq!(hidden, 1);
    assert_eq!(vision.watcher_count(), 1);
    // An observer sees all four.
    let hidden = visible_ids(&state, &state.rules(), 0, &OpenGround, &mut vision, &mut ids);
    assert_eq!((ids.len(), hidden), (4, 0));
    assert!(vision.is_observer());
}

/// The per-cell test of the source's full scan, for comparison.
fn full_scan(grid: &FogGrid, x: f32, z: f32, radius: f32) -> Vec<u8> {
    let mut seen = grid.seen.clone();
    let r2 = radius * radius;
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let cx = grid.origin[0] + (col as f32 + 0.5) * grid.cell_m;
            let cz = grid.origin[1] + (row as f32 + 0.5) * grid.cell_m;
            let dx = cx - x;
            let dz = cz - z;
            if dx * dx + dz * dz <= r2 {
                seen[row * grid.cols + col] = 1;
            }
        }
    }
    seen
}

#[test]
fn bounded_stamp_marks_the_same_cells_as_the_full_scan() {
    let mut mix = Mix(0x57A3);
    let mut grid = FogGrid::covering(1025.0, 777.0);
    for i in 0..400 {
        let x = mix.range(-1300.0, 1300.0);
        let z = mix.range(-1000.0, 1000.0);
        // Radii that put centers exactly on the rim: multiples of half a cell, and random ones.
        let radius = if i % 3 == 0 {
            (mix.below(20) as f32) * 16.0
        } else {
            mix.range(0.0, 500.0)
        };
        let expect = full_scan(&grid, x, z, radius);
        grid.stamp(x, z, radius);
        assert_eq!(grid.seen, expect, "stamp {i} at ({x}, {z}) r {radius}");
        if i % 50 == 49 {
            grid.clear();
        }
    }
    // Cell-center positions and exact-rim radii.
    for (x, z, radius) in [
        (16.0, 16.0, 32.0),
        (-1040.0, 0.0, 64.0),
        (0.0, 0.0, -48.0),
        (5000.0, 0.0, 4000.0),
    ] {
        grid.clear();
        let expect = full_scan(&grid, x, z, radius);
        grid.stamp(x, z, radius);
        assert_eq!(grid.seen, expect, "stamp at ({x}, {z}) r {radius}");
    }
    // Non-finite input behaves as the full scan does.
    for (x, z, radius) in [
        (f32::NAN, 0.0, 10.0),
        (0.0, 0.0, f32::INFINITY),
        (f32::INFINITY, 0.0, 1.0),
    ] {
        grid.clear();
        let expect = full_scan(&grid, x, z, radius);
        grid.stamp(x, z, radius);
        assert_eq!(grid.seen, expect, "stamp at ({x}, {z}) r {radius}");
    }
}

#[test]
fn stamp_team_marks_own_and_allied_watchers_only() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    state.stamp(301, SightClass::Unit, 3, "scout", [500.0, 0.0, 0.0]);
    state.stamp(201, SightClass::Unit, 2, "scout", [-500.0, 0.0, 0.0]);
    state.set_allied(1, 3, true);
    let mut grid = FogGrid::covering(640.0, 640.0);
    grid.stamp_team(&state, &state.rules(), 1);
    assert!(grid.seen_at(0.0, 0.0));
    assert!(grid.seen_at(500.0, 0.0));
    assert!(!grid.seen_at(-500.0, 0.0));
    assert!(grid.seen_count() > 0);
}

#[test]
fn grid_settings_are_checked() {
    assert_eq!(
        FogGrid::try_covering(100.0, 100.0, 0.0, 640.0),
        Err(FogGridError::BadCell(0.0))
    );
    assert!(matches!(
        FogGrid::try_covering(100.0, 100.0, f32::NAN, 640.0),
        Err(FogGridError::BadCell(_))
    ));
    assert!(matches!(
        FogGrid::try_covering(1.0e6, 1.0e6, 1.0, 640.0),
        Err(FogGridError::TooManyCells { .. })
    ));
    let fine = FogGrid::try_covering(100.0, 50.0, 10.0, 640.0);
    assert!(matches!(&fine, Ok(g) if g.cols == 20 && g.rows == 10 && g.origin == [-100.0, -50.0]));
    // A tiny half falls back to the given fallback.
    let fallback = FogGrid::try_covering(0.0, 0.0, 10.0, 200.0);
    assert!(matches!(&fallback, Ok(g) if g.cols == 40 && g.origin == [-200.0, -200.0]));
    // The infallible form falls back instead of allocating a huge grid.
    let huge = FogGrid::covering(1.0e30, 1.0e30);
    assert!(huge.cols * huge.rows <= FOG_GRID_MAX_CELLS);
    assert_eq!(huge.cols, 40);
}

// ---------------------------------------------------------------------------------------------
// Fog v2 (plan ai-nav-18): kind index, indexed vision, three-state cells, ghosts, stealth.
// ---------------------------------------------------------------------------------------------

mod v2 {
    use super::*;
    use regia_detsim::fog::{
        CellState, CompiledSight, ElevationRule, FogCounters, FogMemory, KindSight, team_detects_point,
    };

    /// `field` with every entity carrying its compiled kind id.
    struct WithIds<'a> {
        field: &'a Field,
        sight: CompiledSight,
    }

    impl FogWorld for WithIds<'_> {
        fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
            self.field
                .fog_entities()
                .map(|e| e.with_kind_id(self.sight.kind_id(e.kind)))
        }

        fn is_allied(&self, a: TeamId, b: TeamId) -> bool {
            self.field.is_allied(a, b)
        }
    }

    /// The test's rule sets: the plain rules, and every v2 trait on (stealth, detectors, memory,
    /// eye heights, the elevation rule).
    fn rule_sets(field: &Field) -> Vec<FogRules> {
        let plain = field.rules();
        let mut full = field.rules();
        full.sight = full
            .sight
            .with_row(KindSight::new("crew", DEFAULT_ON_FOOT_SIGHT_M).stealthed())
            .with_row(KindSight::new("watchtower", 120.0).detector(90.0).remembered())
            .with_row(KindSight::new("probe", 0.0).detector(200.0));
        full.sight.unit_eye_m = 1.5;
        full.sight.building_eye_m = 6.0;
        full.elevation = Some(ElevationRule {
            step_m: 2.0,
            low_ground_scale: 0.5,
        });
        vec![plain, full]
    }

    /// Compares every answer of an indexed vision with the free functions (the linear scan).
    fn check_world(field: &Field, label: &str, points: usize) {
        let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 37.5) == (to[0] < 37.5);
        for (set, rules) in rule_sets(field).into_iter().enumerate() {
            for viewer in 0..=6 {
                let indexed = TeamVision::default()
                    .with_index_min_watchers(0)
                    .tap_rebuild(field, &rules, viewer);
                let linear = TeamVision::default()
                    .with_index_min_watchers(usize::MAX)
                    .tap_rebuild(field, &rules, viewer);
                assert!(
                    indexed.is_observer() || indexed.is_indexed(),
                    "{label} viewer {viewer}"
                );
                assert!(!linear.is_indexed());
                for entity in field.fog_entities() {
                    for (name, los) in [("open", &OpenGround as &dyn LineOfSight), ("wall", &wall)] {
                        let expect = entity_visible_to_team(field, &rules, viewer, &entity, los);
                        assert_eq!(
                            indexed.entity_visible(field, &entity, los),
                            expect,
                            "{label} set {set} viewer {viewer} entity {} ({name}, indexed)",
                            entity.id
                        );
                        assert_eq!(
                            linear.entity_visible(field, &entity, los),
                            expect,
                            "{label} set {set} viewer {viewer} entity {} ({name}, linear)",
                            entity.id
                        );
                    }
                }
                let mut mix = Mix(0xF06 ^ viewer as u64);
                for _ in 0..points {
                    let p = [
                        mix.range(-700.0, 700.0),
                        mix.range(-6.0, 6.0),
                        mix.range(-700.0, 700.0),
                    ];
                    let expect = team_sees_point(field, &rules, viewer, p, &OpenGround);
                    assert_eq!(indexed.sees_point(p, &OpenGround), expect, "{label} point {p:?}");
                    assert_eq!(linear.sees_point(p, &OpenGround), expect, "{label} point {p:?}");
                }
            }
        }
    }

    trait TapRebuild {
        fn tap_rebuild(self, field: &Field, rules: &FogRules, viewer: TeamId) -> Self;
    }

    impl TapRebuild for TeamVision {
        fn tap_rebuild(mut self, field: &Field, rules: &FogRules, viewer: TeamId) -> Self {
            self.rebuild(field, rules, viewer);
            self
        }
    }

    #[test]
    fn indexed_vision_equals_the_linear_vision() {
        // F's own oracle: 24 seeded worlds x 7 viewers.
        for seed in 0..24u64 {
            check_world(&random_field(seed, 60), &format!("seed {seed}"), 40);
        }
        // Edge rows: watchers on cell edges, at the radius rim, non-finite positions.
        let mut edges = random_field(99, 30);
        edges.stamp(1001, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
        edges.stamp(
            1002,
            SightClass::Unit,
            2,
            "scout",
            [DEFAULT_UNIT_SIGHT_M, 0.0, 0.0],
        );
        edges.stamp(1003, SightClass::Unit, 1, "scout", [f32::NAN, 0.0, 0.0]);
        edges.stamp(1004, SightClass::Unit, 1, "scout", [f32::INFINITY, 0.0, 5.0]);
        edges.stamp(
            1005,
            SightClass::Unit,
            2,
            "scout",
            [-DEFAULT_UNIT_SIGHT_M, 0.0, 0.0],
        );
        check_world(&edges, "edges", 40);
    }

    /// An RTS-shaped world: `n` entities over +-2000 m, four teams, mostly units, a few buildings
    /// and crews, one radar mast in a hundred, a few neutral features.
    pub(super) fn rts_field(seed: u64, n: u64) -> Field {
        let mut mix = Mix(seed);
        let mut field = Field::new();
        for id in 0..n {
            let roll = mix.below(100);
            let (class, kind) = match roll {
                0 => (SightClass::Building, "radar_mast"),
                1..=14 => (SightClass::Building, "watchtower"),
                15..=29 => (SightClass::OnFoot, "crew"),
                30..=32 => (SightClass::Feature, "vent"),
                _ => (SightClass::Unit, "scout"),
            };
            let team = if class == SightClass::Feature {
                0
            } else {
                1 + mix.below(4) as TeamId
            };
            let position = [
                mix.range(-2000.0, 2000.0),
                mix.range(-5.0, 5.0),
                mix.range(-2000.0, 2000.0),
            ];
            field.stamp(id + 1, class, team, kind, position);
        }
        field
    }

    #[test]
    fn indexed_vision_equals_the_linear_vision_at_900_entities() {
        for seed in [7u64, 8] {
            for (shape, field) in [("rts", rts_field(seed, 900)), ("random", random_field(seed, 900))] {
                let rules = field.rules();
                for viewer in 1..=4 {
                    let indexed = TeamVision::new(&field, &rules, viewer);
                    assert!(indexed.is_indexed(), "900 entities reach the index threshold");
                    let mut counters = FogCounters::default();
                    let mut visible = 0usize;
                    for entity in field.fog_entities() {
                        let expect = entity_visible_to_team(&field, &rules, viewer, &entity, &OpenGround);
                        assert_eq!(
                            indexed.entity_visible_counted(&field, &entity, &OpenGround, &mut counters),
                            expect,
                            "{shape} seed {seed} viewer {viewer} entity {}",
                            entity.id
                        );
                        visible += usize::from(expect);
                    }
                    assert!(visible > 0);
                    if shape == "rts" {
                        // The index tests far fewer watchers than queries x watchers.
                        let all_pairs = counters.queries * indexed.watcher_count() as u64;
                        assert!(
                            counters.index_candidates * 4 < all_pairs,
                            "candidates {} of {all_pairs}",
                            counters.index_candidates
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn kind_index_equals_the_string_lookup() {
        let mut table = SightTable::default()
            .with_kind("radar_mast", 400.0)
            .with_kind("decoy", 0.0)
            .with_row(KindSight::new("cloak", 80.0).stealthed())
            .with_kind("beacon", 333.0);
        // A repeated kind pushed by hand: the first row wins on both paths.
        table.kinds.push(KindSight::new("decoy", 999.0));
        let compiled = table.compile();
        assert_eq!(compiled.len(), 5);
        let kinds = [
            "radar_mast",
            "decoy",
            "cloak",
            "beacon",
            "scout",
            "",
            "Radar_mast",
        ];
        let classes = [
            SightClass::Unit,
            SightClass::OnFoot,
            SightClass::Building,
            SightClass::Feature,
        ];
        for kind in kinds {
            let id = compiled.kind_id(kind);
            assert_eq!(id.is_some(), table.kind_row(kind).is_some(), "{kind}");
            if let Some(id) = id {
                assert_eq!(compiled.row(id), table.kind_row(kind), "{kind}");
            }
            for class in classes {
                let plain = FogEntity::new(1, kind, class, 1, [0.0; 3]);
                let by_id = plain.with_kind_id(id);
                let expect = table.radius_m(kind, class);
                assert_eq!(
                    compiled.radius_m(&plain).to_bits(),
                    expect.to_bits(),
                    "{kind} {class:?}"
                );
                assert_eq!(
                    compiled.radius_m(&by_id).to_bits(),
                    expect.to_bits(),
                    "{kind} {class:?}"
                );
            }
        }
        // An id past the table falls back to the string.
        let stray = FogEntity::new(1, "beacon", SightClass::Unit, 1, [0.0; 3]).with_kind_id(Some(4000));
        assert_eq!(compiled.radius_m(&stray), 333.0);
        // Whole worlds: entities carrying ids answer as the string path does.
        for seed in 0..8u64 {
            let field = random_field(seed, 80);
            for rules in rule_sets(&field) {
                let ids = WithIds {
                    field: &field,
                    sight: rules.sight.compile(),
                };
                for viewer in 0..=5 {
                    let by_string = TeamVision::new(&field, &rules, viewer);
                    let by_id = TeamVision::new(&ids, &rules, viewer);
                    assert_eq!(by_string.watcher_count(), by_id.watcher_count());
                    assert_eq!(by_string.detector_count(), by_id.detector_count());
                    for (a, b) in field.fog_entities().zip(ids.fog_entities()) {
                        assert!(b.kind_id.is_some() == rules.sight.kind_row(a.kind).is_some());
                        assert_eq!(
                            by_string.entity_visible(&field, &a, &OpenGround),
                            by_id.entity_visible(&ids, &b, &OpenGround),
                            "seed {seed} viewer {viewer} entity {}",
                            a.id
                        );
                    }
                }
            }
        }
    }

    /// Team 1's scout, team 2's remembered depot 100 m east.
    fn depot_field() -> (Field, FogRules) {
        let mut state = Field::new();
        state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
        state.stamp(202, SightClass::Building, 2, "depot", [100.0, 0.0, 0.0]);
        state.stamp(203, SightClass::Unit, 2, "scout", [90.0, 0.0, 10.0]);
        let mut rules = state.rules();
        rules.sight = rules
            .sight
            .with_row(KindSight::new("depot", DEFAULT_BUILDING_SIGHT_M).remembered());
        (state, rules)
    }

    #[test]
    fn a_seen_building_leaves_a_ghost_until_its_cell_is_seen_empty() {
        let (mut state, rules) = depot_field();
        let mut memory = FogMemory::new();
        let mut vision = TeamVision::default();
        vision.rebuild(&state, &rules, 1);
        memory.update(&state, &vision, &OpenGround, 1);
        // Only the remembered kind leaves a ghost, not the scout next to it.
        let ghosts = memory.ghosts(1);
        assert_eq!(ghosts.len(), 1);
        assert_eq!((ghosts[0].id, ghosts[0].tick), (202, 1));
        assert_eq!(ghosts[0].position, [100.0, 0.0, 0.0]);
        assert_eq!(ghosts[0].kind, "depot");
        // The scout leaves: the depot is hidden, the ghost stays with the old sighting.
        state.row_mut(101).position = [-1000.0, 0.0, 0.0];
        vision.rebuild(&state, &rules, 1);
        assert!(!vision.entity_visible(&state, &state.by_id(202), &OpenGround));
        memory.update(&state, &vision, &OpenGround, 2);
        assert_eq!(memory.ghosts(1).len(), 1);
        assert_eq!(memory.ghosts(1)[0].tick, 1);
        // Unseen, the depot is rebuilt elsewhere: the ghost keeps only what was seen (never a live
        // field).
        state.row_mut(202).position = [300.0, 0.0, 300.0];
        vision.rebuild(&state, &rules, 1);
        memory.update(&state, &vision, &OpenGround, 3);
        assert_eq!(memory.ghosts(1)[0].position, [100.0, 0.0, 0.0]);
        assert_eq!(memory.ghosts(1)[0].tick, 1);
        // Team 2 holds no ghosts of its own: memory is per viewer.
        assert!(memory.ghosts(2).is_empty());
        // The scout returns and sees the old spot empty: the ghost is dropped.
        state.row_mut(101).position = [0.0, 0.0, 0.0];
        vision.rebuild(&state, &rules, 1);
        let counters = memory.update(&state, &vision, &OpenGround, 4);
        assert!(memory.ghosts(1).is_empty());
        assert_eq!((counters.dropped, counters.ghosts), (1, 0));
    }

    #[test]
    fn a_ghost_stays_while_the_building_is_still_there_and_goes_when_seen_dead() {
        let (mut state, rules) = depot_field();
        let mut memory = FogMemory::new();
        let mut vision = TeamVision::new(&state, &rules, 1);
        memory.update(&state, &vision, &OpenGround, 1);
        state.row_mut(101).position = [-1000.0, 0.0, 0.0];
        vision.rebuild(&state, &rules, 1);
        memory.update(&state, &vision, &OpenGround, 2);
        // Back in sight with the depot still standing: refreshed, not dropped.
        state.row_mut(101).position = [0.0, 0.0, 0.0];
        vision.rebuild(&state, &rules, 1);
        let counters = memory.update(&state, &vision, &OpenGround, 3);
        assert_eq!((counters.refreshed, counters.dropped), (1, 0));
        assert_eq!(memory.ghosts(1)[0].tick, 3);
        // Seen destroyed: dropped.
        state.row_mut(202).alive = false;
        vision.rebuild(&state, &rules, 1);
        memory.update(&state, &vision, &OpenGround, 4);
        assert!(memory.ghosts(1).is_empty());
        // Observers and fog off keep nothing.
        let observer = TeamVision::new(&state, &rules, 0);
        assert_eq!(memory.update(&state, &observer, &OpenGround, 5).ghosts, 0);
        assert_eq!(memory.ghost_count(), 0);
    }

    #[test]
    fn cells_report_unexplored_explored_visible() {
        let mut state = Field::new();
        state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
        state.stamp(301, SightClass::Unit, 3, "scout", [500.0, 0.0, 0.0]);
        state.stamp(201, SightClass::Unit, 2, "scout", [-500.0, 0.0, 0.0]);
        state.set_allied(1, 3, true);
        let rules = state.rules();
        let mut grid = FogGrid::covering(640.0, 640.0);
        assert_eq!(grid.state_at(0.0, 0.0), Some(CellState::Unexplored));
        assert_eq!(grid.state_at(5000.0, 0.0), None);
        let mut vision = TeamVision::new(&state, &rules, 1);
        grid.update(&vision, &OpenGround);
        assert_eq!(grid.state_at(0.0, 0.0), Some(CellState::Visible));
        assert_eq!(
            grid.state_at(500.0, 0.0),
            Some(CellState::Visible),
            "an ally's sight"
        );
        assert_eq!(
            grid.state_at(-500.0, 0.0),
            Some(CellState::Unexplored),
            "an enemy's"
        );
        // With open ground the visible layer is the radius stamp of the same watchers.
        let mut stamped = FogGrid::covering(640.0, 640.0);
        stamped.stamp_team(&state, &rules, 1);
        assert_eq!(grid.visible, stamped.seen);
        // The scout moves on: its old ground is explored, not visible.
        state.row_mut(101).position = [0.0, 0.0, -450.0];
        vision.rebuild(&state, &rules, 1);
        grid.update(&vision, &OpenGround);
        assert_eq!(grid.state_at(0.0, 0.0), Some(CellState::Explored));
        assert_eq!(grid.state_at(0.0, -450.0), Some(CellState::Visible));
        assert!(grid.seen_count() > grid.visible_count());
        assert_eq!(grid.state(0, 0), Some(CellState::Unexplored));
        assert_eq!(grid.state(grid.cols, 0), None);
        // A wall at x = 37.5 hides the cells past it from the scout at the origin.
        state.row_mut(101).position = [0.0, 0.0, 0.0];
        vision.rebuild(&state, &rules, 1);
        let mut walled = FogGrid::covering(640.0, 640.0);
        let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 37.5) == (to[0] < 37.5);
        let calls = walled.update(&vision, &wall);
        assert!(calls > 0);
        assert!(walled.visible_at(16.0, 16.0));
        assert!(!walled.visible_at(80.0, 16.0));
        // An observer sees every cell.
        let observer = TeamVision::new(&state, &rules, 0);
        walled.update(&observer, &wall);
        assert_eq!(walled.visible_count(), walled.cols * walled.rows);
    }

    #[test]
    fn a_stealthed_unit_shows_only_inside_a_detector() {
        let mut state = Field::new();
        state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
        state.stamp(202, SightClass::Unit, 2, "cloak", [50.0, 0.0, 0.0]);
        let mut rules = state.rules();
        rules.sight = rules
            .sight
            .with_row(KindSight::new("cloak", DEFAULT_UNIT_SIGHT_M).stealthed())
            .with_row(KindSight::new("detector_van", DEFAULT_UNIT_SIGHT_M).detector(80.0));
        let shown = |state: &Field, rules: &FogRules, los: &dyn LineOfSight| {
            let free = entity_visible_to_team(state, rules, 1, &state.by_id(202), los);
            let vision = TeamVision::new(state, rules, 1);
            assert_eq!(vision.entity_visible(state, &state.by_id(202), los), free);
            free
        };
        // In plain sight, but stealthed: hidden.
        assert!(!shown(&state, &rules, &OpenGround));
        // A detector 120 m away (outside its 80 m radius): still hidden.
        state.stamp(102, SightClass::Unit, 1, "detector_van", [170.0, 0.0, 0.0]);
        assert!(!shown(&state, &rules, &OpenGround));
        // Within 80 m: shown.
        state.row_mut(102).position = [110.0, 0.0, 0.0];
        assert!(shown(&state, &rules, &OpenGround));
        assert!(team_detects_point(
            &state,
            &rules,
            1,
            [50.0, 0.0, 0.0],
            &OpenGround
        ));
        // A wall between the detector and the cloak hides it again.
        let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 80.0) == (to[0] < 80.0);
        assert!(!shown(&state, &rules, &wall));
        // An enemy's detector does nothing for team 1.
        state.row_mut(102).team = 2;
        assert!(!shown(&state, &rules, &OpenGround));
        // The dead always show; an observer sees it; without the stealth row it is a plain unit.
        state.row_mut(202).alive = false;
        assert!(shown(&state, &rules, &OpenGround));
        state.row_mut(202).alive = true;
        assert!(entity_visible_to_team(
            &state,
            &rules,
            0,
            &state.by_id(202),
            &OpenGround
        ));
        assert!(state.visible(1, 202));
    }

    #[test]
    fn the_elevation_rule_shortens_sight_toward_higher_ground_only() {
        let mut state = Field::new();
        state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
        state.stamp(202, SightClass::Unit, 2, "scout", [100.0, 10.0, 0.0]);
        state.stamp(203, SightClass::Unit, 2, "scout", [0.0, -10.0, 100.0]);
        let mut rules = state.rules();
        assert!(entity_visible_to_team(
            &state,
            &rules,
            1,
            &state.by_id(202),
            &OpenGround
        ));
        rules.elevation = Some(ElevationRule {
            step_m: 5.0,
            low_ground_scale: 0.5,
        });
        // 10 m up: radius 75 m, the unit at 100 m is hidden; the one 10 m down still shows.
        assert!(!entity_visible_to_team(
            &state,
            &rules,
            1,
            &state.by_id(202),
            &OpenGround
        ));
        assert!(entity_visible_to_team(
            &state,
            &rules,
            1,
            &state.by_id(203),
            &OpenGround
        ));
        let vision = TeamVision::new(&state, &rules, 1).with_index_min_watchers(0);
        assert!(!vision.entity_visible(&state, &state.by_id(202), &OpenGround));
        // A scale out of range is clamped (2.0 reads as 1.0: the rule is off).
        rules.elevation = Some(ElevationRule {
            step_m: 5.0,
            low_ground_scale: 2.0,
        });
        assert!(entity_visible_to_team(
            &state,
            &rules,
            1,
            &state.by_id(202),
            &OpenGround
        ));
        rules.elevation = Some(ElevationRule {
            step_m: 5.0,
            low_ground_scale: f32::NAN,
        });
        assert!(!entity_visible_to_team(
            &state,
            &rules,
            1,
            &state.by_id(202),
            &OpenGround
        ));
    }

    #[test]
    fn eye_heights_raise_only_the_hook_question() {
        use std::cell::Cell;
        let asked = Cell::new([0.0f32; 3]);
        let spy = |from: [f32; 3], _to: [f32; 3]| {
            asked.set(from);
            true
        };
        let mut state = Field::new();
        state.stamp(101, SightClass::Building, 1, "watchtower", [0.0, 3.0, 0.0]);
        state.stamp(202, SightClass::Unit, 2, "scout", [50.0, 0.0, 0.0]);
        let mut rules = state.rules();
        assert!(entity_visible_to_team(&state, &rules, 1, &state.by_id(202), &spy));
        assert_eq!(
            asked.get(),
            [0.0, 3.0, 0.0],
            "no eye height: the position as reported"
        );
        rules.sight.building_eye_m = 12.0;
        assert!(entity_visible_to_team(&state, &rules, 1, &state.by_id(202), &spy));
        assert_eq!(asked.get(), [0.0, 15.0, 0.0]);
        let mut bad = rules.sight.clone();
        bad.unit_eye_m = -1.0;
        assert_eq!(
            bad.first_invalid().map(|(w, _)| w),
            Some("unit_eye_m".to_string())
        );
        let bad = SightTable::default().with_row(KindSight::new("probe", 10.0).detector(f32::NAN));
        assert_eq!(
            bad.first_invalid().map(|(w, _)| w),
            Some("kinds[probe].detector_m".to_string())
        );
    }
}

/// Fog presentation (ai-nav-19): the grid's dirty-row bitset names exactly the rows whose cell
/// states changed, so a fog texture uploads only those.
#[test]
fn grid_marks_only_changed_rows_dirty() {
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    let rules = state.rules();
    let mut grid = FogGrid::covering(640.0, 640.0);
    assert_eq!(grid.dirty_row_count(), grid.rows, "a new grid is all dirty");
    let mut vision = TeamVision::new(&state, &rules, 1);
    grid.update(&vision, &OpenGround);
    grid.clear_dirty_rows();
    assert_eq!(grid.dirty_row_count(), 0);
    // The same view again changes nothing.
    grid.update(&vision, &OpenGround);
    assert_eq!(grid.dirty_row_count(), 0, "an unchanged view dirties no row");
    // Snapshot the states, move the scout north, and compare row by row.
    let before: Vec<Vec<_>> = (0..grid.rows)
        .map(|r| (0..grid.cols).map(|c| grid.state(c, r)).collect())
        .collect();
    state.row_mut(101).position = [0.0, 0.0, -200.0];
    vision.rebuild(&state, &rules, 1);
    grid.update(&vision, &OpenGround);
    let mut changed = 0;
    for (r, row) in before.iter().enumerate() {
        let now: Vec<_> = (0..grid.cols).map(|c| grid.state(c, r)).collect();
        assert_eq!(grid.is_row_dirty(r), &now != row, "row {r}");
        changed += usize::from(&now != row);
    }
    assert!(
        changed > 0 && changed < grid.rows,
        "{changed} of {} rows",
        grid.rows
    );
    // Stamps and clears keep the bitset too; equality ignores it.
    grid.clear_dirty_rows();
    let copy = grid.clone();
    grid.stamp(600.0, 600.0, 40.0);
    assert!(grid.dirty_row_count() >= 1);
    grid.clear_dirty_rows();
    grid.stamp(600.0, 600.0, 40.0);
    assert_eq!(
        grid.dirty_row_count(),
        0,
        "re-stamping seen cells changes nothing"
    );
    let mut dirty_copy = copy.clone();
    dirty_copy.mark_all_dirty();
    assert_eq!(dirty_copy, copy);
    grid.clear();
    assert_eq!(grid.dirty_row_count(), grid.rows);
    assert!(!grid.is_row_dirty(grid.rows));
}

/// KI-GP-018 (ADR-10070): with the ground under each cell the elevation rule applies to fog cells as
/// it does to entities: a cell on ground more than `step_m` above the watcher is seen only inside the
/// scaled radius; low or level ground keeps the full radius. Without the rule, `update_on_ground` is
/// exactly `update`.
#[test]
fn the_elevation_rule_applies_to_fog_cells_on_the_ground() {
    use regia_detsim::fog::{CellState, ElevationRule};

    let r = DEFAULT_UNIT_SIGHT_M;
    let mut state = Field::new();
    state.stamp(101, SightClass::Unit, 1, "scout", [0.0, 0.0, 0.0]);
    // A 10 m plateau east of x = 0.
    let ground = |x: f32, _z: f32| if x > 0.0 { 10.0 } else { 0.0 };
    let mut rules = state.rules();

    let vision = TeamVision::new(&state, &rules, 1);
    let mut flat = FogGrid::covering(640.0, 640.0);
    let mut lifted = FogGrid::covering(640.0, 640.0);
    flat.update(&vision, &OpenGround);
    lifted.update_on_ground(&vision, &OpenGround, ground);
    assert_eq!(flat, lifted, "no elevation rule: the same cells");

    rules.elevation = Some(ElevationRule {
        step_m: 5.0,
        low_ground_scale: 0.5,
    });
    let vision = TeamVision::new(&state, &rules, 1);
    let mut grid = FogGrid::covering(640.0, 640.0);
    grid.update_on_ground(&vision, &OpenGround, ground);
    let at = |g: &FogGrid, x: f32| g.state_at(x, 0.0);
    assert_eq!(
        at(&grid, -0.8 * r),
        Some(CellState::Visible),
        "level ground: full radius"
    );
    assert_eq!(
        at(&grid, 0.3 * r),
        Some(CellState::Visible),
        "high ground inside half the radius"
    );
    assert_eq!(
        at(&grid, 0.8 * r),
        Some(CellState::Unexplored),
        "high ground past half the radius is hidden"
    );
    // The plain update ignores the rule on cells (the old answer).
    let mut plain = FogGrid::covering(640.0, 640.0);
    plain.update(&vision, &OpenGround);
    assert_eq!(at(&plain, 0.8 * r), Some(CellState::Visible));
    // An observer sees every cell either way.
    let observer = TeamVision::new(&state, &rules, 0);
    let mut all = FogGrid::covering(640.0, 640.0);
    all.update_on_ground(&observer, &OpenGround, ground);
    assert!(all.visible.iter().all(|&v| v == 1));
}
