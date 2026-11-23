// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/grid.rs, commit d6ed37677), ADR-8012:
// its two map-size tests (lines 957-990). Also from ir-sim/src/pathing/tests.rs (commit d6ed37677):
// the grid-only tests at lines 107-135, 173-207 and 882-964, with the `terrain` fixture at
// lines 13-32. Mark: adapt (ADR-8049): the fixtures build a `HeightCells` instead of the game's
// heightfield type (the same quantized heights: 1/32 m counts around a base count), the profiles
// and classes come from `GridSettings::hover_ground_jet`, and names that pointed at the game's
// maps were made neutral. The other tests in this file are new (plan row S-01).

use std::sync::Arc;

use super::*;

const SIDE: usize = 256;
const WIDTH: f32 = 1280.0;
const BASE: u16 = 16384;
/// Meters per raw height count in the fixtures.
const M_PER_COUNT: f32 = 1.0 / 32.0;

fn settings() -> Arc<GridSettings> {
    Arc::new(GridSettings::hover_ground_jet())
}

/// `validate` allows a canvas of `i32::MAX` by 2 (`2 * i32::MAX` cells is still
/// `<= u32::MAX`). `segments_from_grid_window` then adds `width as i32 + 1`.
/// A center far off the map never reads a cell; the add runs first.
#[test]
fn max_legal_canvas_wall_window_does_not_overflow() {
    fn shell(width: usize, height: usize) -> NavGrid {
        let settings = settings();
        let mut key = GroundKey::flat_canvas(&settings);
        key.width = width;
        key.height = height;
        let stat = Arc::new(NavStatic {
            width,
            height,
            cell_m: 1.0,
            origin_x: 0.0,
            origin_z: 0.0,
            cost: Vec::new(),
            edges: Vec::new(),
            steep: Vec::new(),
            height_cells: Vec::new(),
            lowered: Vec::new(),
            key,
            settings,
        });
        NavGrid {
            stat,
            blockers: Vec::new(),
            labels: Vec::new(),
            wide_labels: Vec::new(),
            regions: Vec::new(),
            clusters_x: 1,
            clusters_z: 1,
            closed: Arc::new(Vec::new()),
            excluded: Vec::new(),
            exclusions: Vec::new(),
            stamps: Vec::new(),
            cluster: 2,
            shift: 1,
            generation: 0,
            cluster_gen: Vec::new(),
            shapes: Vec::new(),
            area: Arc::new(Vec::new()),
            area_pct: Arc::new(Vec::new()),
            overlay: None,
            sync_dirty: Vec::new(),
            dirty_clusters: Vec::new(),
            poly_scratch: Vec::new(),
            flood_stack: Vec::new(),
        }
    }

    let mut out = Vec::new();
    // Off the map on both axes, so a correct clamp touches only the outside ring.
    let center = [-1.0e20, -1.0e20];
    let wide = shell(i32::MAX as usize, 2);
    crate::walls::segments_from_grid_window(&wide, Profile::FOOT, center, 2.0, &mut out);
    assert!(out.is_empty(), "width i32::MAX: {out:?}");
    let tall = shell(2, i32::MAX as usize);
    crate::walls::segments_from_grid_window(&tall, Profile::FOOT, center, 2.0, &mut out);
    assert!(out.is_empty(), "height i32::MAX: {out:?}");
}

/// Heights quantized like the source fixtures: raw counts of 1/32 m around `BASE`.
fn quantized(cols: usize, rows: usize, cell_m: f32, h: impl Fn(usize, usize) -> f32) -> HeightCells {
    let mut heights = Vec::with_capacity(cols * rows);
    for z in 0..rows {
        for x in 0..cols {
            let counts = (BASE as f32 + h(x, z) * 32.0).round() as u16;
            heights.push(counts as f32 * M_PER_COUNT);
        }
    }
    let mut cells = HeightCells::centered(cols, rows, cell_m, heights);
    // Absolute heights: 0 m is BASE counts.
    cells.center_height = BASE as f32 * M_PER_COUNT;
    cells
}

/// Heightfield from a function of world (x, z) in meters. 5 m cells over 1280 m.
fn terrain(height: impl Fn(f32, f32) -> f32) -> HeightCells {
    let spacing = WIDTH / SIDE as f32;
    let half = WIDTH * 0.5;
    quantized(SIDE, SIDE, spacing, |x, z| {
        let wx = x as f32 * spacing - half;
        let wz = z as f32 * spacing - half;
        det_clamp(height(wx, wz), -400.0, 400.0)
    })
}

fn build(ground: &HeightCells) -> NavGrid {
    let stat = NavStatic::build(Some(ground), settings()).expect("valid ground");
    NavGrid::new(Arc::new(stat)).expect("grid")
}

#[test]
fn negative_zero_patch_has_the_same_signature_and_grid_hash() {
    let ground = |zero| {
        HeightCells::centered(2, 2, 5.0, vec![0.0; 4]).with_patches(vec![GroundPatch {
            origin: [-5.0, -5.0],
            spacing: 5.0,
            cols: 2,
            rows: 2,
            heights: vec![zero; 4],
        }])
    };
    let positive = ground(std::hint::black_box(0.0));
    let negative = ground(std::hint::black_box(-0.0));
    assert_eq!(
        negative.patch_signature(),
        positive.patch_signature(),
        "a patch's zero sign must not change its cache key"
    );
    let positive = NavStatic::build(Some(&positive), settings()).expect("positive zero patch");
    let negative = NavStatic::build(Some(&negative), settings()).expect("negative zero patch");
    assert_eq!(negative.bits_hash(), positive.bits_hash());
    assert!(negative.height_cells.iter().all(|h| h.to_bits() == 0));
}

#[test]
fn negative_zero_patch_selection_stores_positive_zero() {
    struct ZeroGround;
    impl GridGround for ZeroGround {
        fn cols(&self) -> usize {
            2
        }
        fn rows(&self) -> usize {
            2
        }
        fn spacing(&self) -> f32 {
            5.0
        }
        fn half_x(&self) -> f32 {
            5.0
        }
        fn half_z(&self) -> f32 {
            5.0
        }
        fn center_height(&self) -> f32 {
            0.0
        }
        fn height_at_index(&self, _x: usize, _z: usize) -> f32 {
            std::hint::black_box(-0.0)
        }
        fn height_at_world(&self, _x: f32, _z: f32) -> f32 {
            std::hint::black_box(-0.0)
        }
        fn has_patches(&self) -> bool {
            true
        }
    }
    let grid = NavStatic::build(Some(&ZeroGround), settings()).expect("zero ground");
    for h in grid.height_cells {
        assert_eq!(h.to_bits(), 0, "patch selection must store canonical zero");
    }
}

#[test]
fn negative_zero_patch_query_returns_positive_zero() {
    // At this corner both bilinear samplers produce -0 before selecting the lower height.
    let heights = vec![-0.0, -1.0, -0.0, -1.0];
    let mut ground = HeightCells::centered(2, 2, 5.0, heights.clone());
    ground.half_x = 0.0;
    ground.half_z = -0.0;
    ground.patches.push(GroundPatch {
        origin: [0.0, 0.0],
        spacing: 5.0,
        cols: 2,
        rows: 2,
        heights,
    });
    let x = std::hint::black_box(0.0);
    let z = std::hint::black_box(-0.0);
    assert_eq!(ground.grid_height_at_world(x, z), 0.0);
    assert_eq!(ground.patches[0].sample(x, z), Some(0.0));
    assert_eq!(ground.height_at_world(x, z).to_bits(), 0);
}

#[test]
fn grid_float_hash_canonicalizes_zero_and_nan_only() {
    let hash = |value| {
        let mut hash = Fnv::new();
        hash.f32(value);
        hash.finish()
    };
    for bits in [0x8000_0000, 0] {
        assert_eq!(hash(f32::from_bits(bits)), hash(0.0));
    }
    for bits in [0x7f80_0001, 0xffc0_1234, 0x7fff_ffff] {
        assert_eq!(hash(f32::from_bits(bits)), hash(f32::from_bits(0x7fc0_0000)));
    }
    for bits in [1, 0x8000_0001, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000] {
        let mut expected = Fnv::new();
        expected.u32(bits);
        assert_eq!(hash(f32::from_bits(bits)), expected.finish());
    }
}

#[test]
fn grid_cache_keys_canonicalize_zero_and_nan() {
    let key = |bits| {
        let mut heights = vec![0.0; 9];
        heights[3] = f32::from_bits(bits); // The diagonal probe samples (0, 1).
        let ground = HeightCells::centered(3, 3, 5.0, heights);
        GroundKey::of(Some(&ground), &settings())
    };
    assert_eq!(key(0x8000_0000), key(0));
    assert_eq!(key(0xffc0_1234), key(0x7fc0_0000));
    let signature = |bits| {
        let heights = [f32::from_bits(bits); 4];
        patch_signature([(2, 2, heights.as_slice())])
    };
    assert_eq!(signature(0x8000_0000), signature(0));
    assert_eq!(signature(0xffc0_1234), signature(0x7fc0_0000));
}

// ------------------------------------------------------------------ ported from the source

#[test]
fn declared_extent_keeps_the_canvas_flat_key_and_covers_2050() {
    let s = settings();
    let canvas = GroundKey::of(None, &s);
    let declared = GroundKey::covering(None, Some([1280.0, 1280.0]), &s);
    assert_eq!(declared, canvas);
    assert_eq!(declared.width, 256);
    assert_eq!(declared.half_x, 640.0);
    assert_eq!(declared.half_z, 640.0);
    let wide = GroundKey::flat_for_extent(2050.0, 2050.0, &s);
    assert_eq!(wide.width, 410);
    assert_eq!(wide.height, 410);
    assert_eq!(wide.half_x, 1025.0);
    assert_eq!(wide.half_z, 1025.0);
    assert_eq!(GroundKey::covering(None, Some([2050.0, 2050.0]), &s), wide);
    let tiny = quantized(4, 4, 5.0, |_, _| 0.0);
    let built = NavStatic::build_sized(Some(&tiny), Some([2050.0, 2050.0]), s.clone()).expect("tiny");
    assert_eq!(built.width, 4, "a loaded ground wins over the declared size");
    assert_eq!(NavStatic::build(None, s).expect("canvas").width, 256);
}

#[test]
fn flat_nav_at_2050_contains_a_point_past_the_canvas_rim() {
    let stat = Arc::new(NavStatic::build_sized(None, Some([2050.0, 2050.0]), settings()).expect("flat"));
    assert_eq!(stat.width, 410);
    let grid = NavGrid::new(stat).expect("grid");
    assert!(grid.in_bounds_world(900.0, 0.0));
    assert!(!grid.in_bounds_world(1100.0, 0.0));
}

#[test]
fn terrain_cost_rises_with_slope_and_cliffs_block() {
    let (hover, ground, jet) = (MoveClass::HOVER, MoveClass::GROUND, MoveClass::JET);
    assert_eq!(hover.cost(0.1), 10);
    assert_eq!(hover.cost(0.2), 10, "under the free band");
    assert!(hover.cost(0.5) > 10, "between free and the 28 degree steep band");
    assert!(hover.cost(0.7) > 20, "a face the hover class can barely hold");
    assert_eq!(hover.cost(0.9), 0, "past grip, about 38 degrees");
    assert_eq!(
        hover.cost(4.1),
        0,
        "a 76 degree face is a cliff for a hover class"
    );
    assert_eq!(ground.cost(1.6), 0, "on foot stops at 1.5");
    assert!(jet.cost(3.0) > 0, "a jetpack climbs a 3.0 step");
    assert_eq!(jet.cost(4.5), 0);
}

#[test]
fn supercover_visits_every_cell_on_the_line_and_both_at_a_corner() {
    let mut seen = Vec::new();
    supercover(0, 0, 3, 1, |_, _, x, z| {
        seen.push((x, z));
        true
    });
    assert_eq!(seen.first(), Some(&(0, 0)));
    assert_eq!(seen.last(), Some(&(3, 1)));
    let mut diag = Vec::new();
    supercover(0, 0, 2, 2, |_, _, x, z| {
        diag.push((x, z));
        true
    });
    assert!(
        diag.contains(&(1, 0)) && diag.contains(&(0, 1)),
        "corner cells count: {diag:?}"
    );
}

/// Rectangular heightfields: the grid keeps per-axis bounds, world mapping and cluster counts. A
/// square assumption (side = max axis) would put `cell_at` over rows that do not exist.
#[test]
fn rectangular_grid_keeps_per_axis_bounds() {
    // 64x32 cells at 5 m -> a 320 x 160 m map (X wide, Z narrow).
    let (cols, rows) = (64usize, 32usize);
    let ground = quantized(cols, rows, 5.0, |_, _| 0.0);
    assert_eq!(ground.cols(), cols);
    assert_eq!(ground.rows(), rows);
    assert!((ground.half_x - 160.0).abs() < 1e-5);
    assert!((ground.half_z - 80.0).abs() < 1e-5);
    let grid = build(&ground);
    assert_eq!(grid.width(), cols);
    assert_eq!(grid.height(), rows);
    assert_eq!(grid.clusters_x, cols.div_ceil(CLUSTER));
    assert_eq!(grid.clusters_z, rows.div_ceil(CLUSTER));
    assert!((grid.stat.origin_x - -160.0).abs() < 1e-5);
    assert!((grid.stat.origin_z - -80.0).abs() < 1e-5);
    // The world center maps to the center cell of EACH axis, not a square's.
    let center = grid.cell_at(0.0, 0.0);
    assert_eq!(grid.xz(center), (cols / 2, rows / 2));
    // In bounds on the last cell of each axis, off-grid past it.
    assert!(grid.in_bounds_world(157.0, 77.0));
    assert!(grid.in_bounds_world(-157.0, -77.0));
    assert!(!grid.in_bounds_world(0.0, 82.0), "past the short Z axis");
    assert!(!grid.in_bounds_world(162.0, 0.0), "past the long X axis");
    // Off-grid world points clamp onto the border, never wrap or panic.
    let clamped = grid.cell_at(500.0, 500.0);
    assert_eq!(grid.xz(clamped), (cols - 1, rows - 1));
}

/// A lowered funnel baked as a ground patch: 0.5 m cells, a 40 m slope that drops 24 m.
fn funnel_patch(drop_over_run: f32) -> GroundPatch {
    let (cols, rows) = (121usize, 41usize);
    let mut heights = Vec::with_capacity(cols * rows);
    for _r in 0..rows {
        for c in 0..cols {
            heights.push(-det_min(c as f32 * 0.5 * drop_over_run, 24.0));
        }
    }
    GroundPatch {
        origin: [0.0, 0.0],
        spacing: 0.5,
        cols,
        rows,
        heights,
    }
}

#[test]
fn a_walkable_mouth_funnel_is_walkable_in_the_nav_grid_and_a_cliff_one_is_not() {
    let flat = terrain(|_, _| 0.0);
    let plain = NavStatic::build(Some(&flat), settings()).expect("plain");
    let key_plain = plain.key;

    let gentle = terrain(|_, _| 0.0).with_patches(vec![funnel_patch(0.6)]);
    let stat = NavStatic::build(Some(&gentle), settings()).expect("gentle");
    assert_ne!(stat.key, key_plain, "a patched map rebuilds the grid");
    let nav = NavGrid::new(Arc::new(stat)).expect("grid");
    let cell = nav.cell_at(30.0, 10.0);
    assert!(
        nav.stat.height_cells[cell as usize] < -10.0,
        "the nav height follows the drawn funnel"
    );
    assert!(
        nav.stat.steep[cell as usize] < 0.75,
        "0.6 slope reads as a slope, not a cliff"
    );
    for profile in [Profile::FOOT, Profile::HOVER] {
        assert!(
            nav.passable(profile, cell),
            "{profile:?} stands on the funnel slope"
        );
        assert!(
            nav.can_step(profile, cell, 1, 0),
            "{profile:?} walks down the funnel"
        );
        assert!(nav.can_step(profile, cell, -1, 0), "{profile:?} walks back up");
    }

    let sheer = terrain(|_, _| 0.0).with_patches(vec![funnel_patch(4.0)]);
    let nav = build(&sheer);
    let cell = nav.cell_at(2.0, 10.0);
    assert!(
        !nav.passable(Profile::FOOT, cell),
        "a 4:1 funnel wall is still a cliff on foot"
    );
}

#[test]
fn funnel_cells_are_flagged_so_the_ramp_cut_leaves_the_slope_open() {
    let plain = NavStatic::build(Some(&terrain(|_, _| 0.0)), settings()).expect("plain");
    assert!(
        plain.lowered.iter().all(|f| *f == 0),
        "no patch, no lowered cells"
    );
    let patched = terrain(|_, _| 0.0).with_patches(vec![funnel_patch(0.6)]);
    let nav = build(&patched);
    let deep = nav.cell_at(40.0, 10.0) as usize;
    let rim = nav.cell_at(-20.0, 10.0) as usize;
    assert_eq!(
        nav.stat.lowered[deep], 1,
        "a cell the patch lowers is a lowered cell"
    );
    assert_eq!(nav.stat.lowered[rim], 0, "ground outside the patch is not");
}

// ------------------------------------------------------------------ the stamp API (S-01)

/// What the source sim knew about an entity when it chose its obstacles.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Building,
    Wreck,
    /// A vehicle parked with nobody driving it.
    ParkedVehicle,
    /// A moving unit: never an obstacle on the grid.
    Moving,
    /// A large debris pile with a square footprint this many meters wide.
    Mound(f32),
}

#[derive(Debug, Clone, Copy)]
struct Ent {
    id: u32,
    kind: Kind,
    dead: bool,
    pos: [f32; 3],
    radius: f32,
}

/// The source's obstacle rule (`obstacle_of`, mod.rs 587-604): buildings, wrecks and parked
/// vehicles that are alive become a disc at their position with their collision radius.
fn class_stamp(e: &Ent) -> Option<Stamp> {
    if e.dead {
        return None;
    }
    let solid = matches!(e.kind, Kind::Building | Kind::Wreck | Kind::ParkedVehicle);
    if !solid {
        return None;
    }
    let p = e.pos;
    let r = e.radius;
    Some(Stamp {
        x_cm: (p[0] * 100.0).round() as i32,
        z_cm: (p[2] * 100.0).round() as i32,
        r_cm: (r * 100.0).round() as i32,
    })
}

/// The source's mound rule (mod.rs 3119-3137): the disc that covers the square footprint.
fn class_mound(e: &Ent) -> Option<Stamp> {
    let Kind::Mound(side) = e.kind else { return None };
    if e.dead {
        return None;
    }
    let (min, max) = (-side * 0.5, side * 0.5);
    let p = e.pos;
    Some(Stamp {
        x_cm: (p[0] * 100.0).round() as i32,
        z_cm: (p[2] * 100.0).round() as i32,
        r_cm: ((max - min) * 72.5).round() as i32,
    })
}

/// The source's per-tick sync (`sync_obstacles`, mod.rs 885-940), class rules inline: it returns
/// the dirty cluster flags. Kept as the reference the API must match bit for bit.
fn class_sync(grid: &mut NavGrid, stamps: &mut Vec<(u32, Stamp)>, ents: &[Ent]) -> Option<Vec<bool>> {
    let mut now: Vec<(u32, Stamp)> = ents
        .iter()
        .filter_map(|e| class_stamp(e).map(|s| (e.id, s)))
        .collect();
    now.extend(ents.iter().filter_map(|e| class_mound(e).map(|s| (e.id, s))));
    now.sort_unstable_by_key(|(id, _)| *id);
    if now == *stamps {
        return None;
    }
    let (cx_n, cz_n) = (grid.clusters_x, grid.clusters_z);
    let mut dirty = vec![false; cx_n * cz_n];
    let mut mark = |span: Option<(usize, usize, usize, usize)>| {
        if let Some((x0, z0, x1, z1)) = span {
            for z in z0..=z1.min(cz_n - 1) {
                for x in x0..=x1.min(cx_n - 1) {
                    dirty[z * cx_n + x] = true;
                }
            }
        }
    };
    let (mut i, mut j) = (0usize, 0usize);
    let old = std::mem::take(stamps);
    while i < old.len() || j < now.len() {
        let take_old = j >= now.len() || (i < old.len() && old[i].0 < now[j].0);
        let take_new = i >= old.len() || (j < now.len() && now[j].0 < old[i].0);
        if take_old {
            mark(grid.stamp(old[i].1, false));
            i += 1;
        } else if take_new {
            mark(grid.stamp(now[j].1, true));
            j += 1;
        } else {
            if old[i].1 != now[j].1 {
                mark(grid.stamp(old[i].1, false));
                mark(grid.stamp(now[j].1, true));
            }
            i += 1;
            j += 1;
        }
    }
    *stamps = now;
    for (c, d) in dirty.iter().enumerate() {
        if *d {
            grid.relabel_cluster(c);
        }
    }
    Some(dirty)
}

/// What a host hands the API: it classifies its own entities and builds the discs with the
/// `Stamp` helpers.
fn api_stamps(ents: &[Ent]) -> Vec<(u32, Stamp)> {
    let mut out = Vec::new();
    for e in ents.iter().rev() {
        if e.dead {
            continue;
        }
        match e.kind {
            Kind::Building | Kind::Wreck | Kind::ParkedVehicle => {
                out.push((e.id, Stamp::from_world(e.pos[0], e.pos[2], e.radius)));
            }
            Kind::Mound(side) => out.push((e.id, Stamp::covering_square(e.pos[0], e.pos[2], side))),
            Kind::Moving => {}
        }
    }
    out
}

/// A fixed-seed LCG (Knuth's MMIX constants) for scenario inputs.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
    /// A float in [lo, hi) from 24 random bits.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * ((self.next() >> 8) as f32 / (1u32 << 24) as f32)
    }
}

/// A ridge and a bowl, so clusters split into several regions per profile.
fn rough_ground() -> HeightCells {
    quantized(96, 80, 5.0, |x, z| {
        let (fx, fz) = (x as f32 - 48.0, z as f32 - 40.0);
        let ridge = if (fx - 10.0).abs() < 2.0 && fz.abs() < 30.0 {
            40.0
        } else {
            0.0
        };
        let bowl = if fx * fx + (fz - 20.0) * (fz - 20.0) < 64.0 {
            -6.0
        } else {
            0.0
        };
        ridge + bowl + (fx * 0.25)
    })
}

fn scenario_ticks() -> Vec<Vec<Ent>> {
    let mut rng = Lcg(0x5eed_0001);
    let kinds = [
        Kind::Building,
        Kind::Wreck,
        Kind::ParkedVehicle,
        Kind::Moving,
        Kind::Mound(8.0),
    ];
    let mut ents: Vec<Ent> = (0..40u32)
        .map(|k| Ent {
            id: 1000 + k * 7,
            kind: kinds[(k % 5) as usize],
            dead: false,
            pos: [rng.range(-260.0, 260.0), 0.0, rng.range(-220.0, 220.0)],
            radius: [0.4, 2.5, 4.5, 8.5, 12.0][(k % 5) as usize],
        })
        .collect();
    // One disc reaching past the map's edge, one wholly off the map.
    ents[3].pos = [238.0, 0.0, -198.0];
    ents[3].kind = Kind::Building;
    ents[8].pos = [400.0, 0.0, 400.0];
    ents[8].kind = Kind::Wreck;
    let mut ticks = vec![ents.clone()];
    for _ in 0..12 {
        for e in &mut ents {
            match rng.next() % 6 {
                0 => {
                    e.pos[0] += rng.range(-9.0, 9.0);
                    e.pos[2] += rng.range(-9.0, 9.0);
                }
                1 => e.dead = !e.dead,
                2 if e.kind == Kind::ParkedVehicle => e.kind = Kind::Moving,
                3 if e.kind == Kind::Moving => e.kind = Kind::ParkedVehicle,
                _ => {}
            }
        }
        ticks.push(ents.clone());
        // An unchanged tick between changes.
        ticks.push(ents.clone());
    }
    ticks
}

fn assert_same_live_state(api: &NavGrid, reference: &NavGrid, tick: usize) {
    assert_eq!(
        api.blockers, reference.blockers,
        "tick {tick}: blocker layers differ"
    );
    assert_eq!(api.labels, reference.labels, "tick {tick}: region labels differ");
    assert_eq!(api.regions, reference.regions, "tick {tick}: region sets differ");
}

#[test]
fn stamp_api_matches_the_class_stamps_bit_for_bit() {
    let ground = rough_ground();
    let stat = Arc::new(NavStatic::build(Some(&ground), settings()).expect("ground"));
    let mut reference = NavGrid::new(stat.clone()).expect("grid");
    let mut api = NavGrid::new(stat).expect("grid");
    let mut ref_stamps: Vec<(u32, Stamp)> = Vec::new();
    let mut changes = 0;
    for (tick, ents) in scenario_ticks().iter().enumerate() {
        let dirty = class_sync(&mut reference, &mut ref_stamps, ents);
        let sync = api.sync_stamps(api_stamps(ents));
        assert_eq!(sync.changed, dirty.is_some(), "tick {tick}: change flag");
        let want: Vec<u32> = dirty
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter(|(_, d)| **d)
            .map(|(c, _)| c as u32)
            .collect();
        assert_eq!(sync.dirty_clusters, want, "tick {tick}: dirty clusters");
        assert_eq!(api.stamps(), ref_stamps.as_slice(), "tick {tick}: stamp set");
        assert_same_live_state(&api, &reference, tick);
        changes += usize::from(sync.changed);
    }
    assert!(
        changes >= 10,
        "the scenario changes the set on most ticks ({changes})"
    );
    assert!(
        api.blockers.iter().all(|l| l.iter().any(|c| *c != 0)),
        "every size layer carries a disc"
    );
}

/// Incremental syncs end where stamping the final set on a fresh grid ends, and each layer's count
/// is the number of inflated discs whose reach covers the cell center.
#[test]
fn sync_stamps_equals_a_fresh_stamp_of_the_same_set() {
    let ground = rough_ground();
    let stat = Arc::new(NavStatic::build(Some(&ground), settings()).expect("ground"));
    let mut live = NavGrid::new(stat.clone()).expect("grid");
    let ticks = scenario_ticks();
    for ents in &ticks {
        live.sync_stamps(api_stamps(ents));
    }
    let last = api_stamps(ticks.last().expect("ticks"));
    let mut fresh = NavGrid::new(stat).expect("grid");
    fresh.sync_stamps(last.clone());
    assert_same_live_state(&live, &fresh, ticks.len());
    assert_eq!(live.state_hash(), fresh.state_hash());

    for (s, class) in live.settings().size_classes.iter().enumerate() {
        for cell in 0..(live.width() * live.height()) as u32 {
            let [cx, cz] = live.center(cell);
            let covering = last
                .iter()
                .filter(|(_, d)| {
                    let reach = d.r_cm as f32 / 100.0 + class.inflate_m;
                    let (dx, dz) = (cx - d.x_cm as f32 / 100.0, cz - d.z_cm as f32 / 100.0);
                    dx * dx + dz * dz < reach * reach
                })
                .count();
            assert_eq!(
                usize::from(live.blockers[s][cell as usize]),
                covering,
                "size class {s}, cell {cell}"
            );
        }
    }
}

#[test]
fn an_unchanged_set_touches_nothing_and_order_does_not_matter() {
    let mut grid = NavGrid::new(Arc::new(NavStatic::build(None, settings()).expect("flat"))).expect("grid");
    let a = (5, Stamp::from_world(10.0, 10.0, 3.0));
    let b = (9, Stamp::from_world(-40.0, 25.0, 6.0));
    let first = grid.sync_stamps(vec![b, a]);
    assert!(first.changed && !first.dirty_clusters.is_empty());
    let hash = grid.state_hash();
    let again = grid.sync_stamps(vec![a, b]);
    assert_eq!(
        again,
        StampSync::default(),
        "the same set in another order is no change"
    );
    assert_eq!(grid.state_hash(), hash);
    let cleared = grid.sync_stamps(Vec::new());
    assert!(cleared.changed);
    assert!(
        grid.blockers.iter().all(|l| l.iter().all(|c| *c == 0)),
        "every disc lifted"
    );
    assert!(grid.stamps().is_empty());
}

// ------------------------------------------------------------------ settings

#[test]
fn cell_size_and_canvas_are_settings() {
    let s = Arc::new(GridSettings {
        cell_m: 2.5,
        canvas_cells: [200, 100],
        ..GridSettings::hover_ground_jet()
    });
    let stat = NavStatic::build(None, s.clone()).expect("canvas");
    assert_eq!((stat.width, stat.height), (200, 100));
    assert_eq!(stat.cell_m, 2.5);
    assert_eq!((stat.origin_x, stat.origin_z), (-250.0, -125.0));
    assert_eq!(s.canvas_m(), [500.0, 250.0]);
    // A declared extent equal to the canvas keeps the canvas key exactly.
    assert_eq!(
        GroundKey::covering(None, Some([500.0, 250.0]), &s),
        GroundKey::flat_canvas(&s)
    );
    // Another extent rounds to whole cells of the setting's size.
    let other = GroundKey::flat_for_extent(101.0, 51.0, &s);
    assert_eq!((other.width, other.height), (40, 20));
    assert_eq!((other.half_x, other.half_z), (50.0, 25.0));
    let grid = NavGrid::new(Arc::new(stat)).expect("grid");
    assert_eq!((grid.clusters_x, grid.clusters_z), (25, 13));
    assert!(grid.in_bounds_world(248.0, 123.0));
    assert!(!grid.in_bounds_world(252.0, 0.0));
}

#[test]
fn size_classes_and_profiles_are_settings() {
    let s = Arc::new(GridSettings {
        cell_m: 1.0,
        canvas_cells: [64, 64],
        move_classes: vec![MoveClass::new(0.2, 0.4, 0.6), MoveClass::new(0.5, 1.0, 2.0)],
        size_classes: vec![
            SizeClass {
                inflate_m: 0.0,
                max_radius_m: 0.5,
            },
            SizeClass {
                inflate_m: 2.0,
                max_radius_m: 2.0,
            },
            SizeClass {
                inflate_m: 5.0,
                max_radius_m: 6.0,
            },
            SizeClass {
                inflate_m: 9.0,
                max_radius_m: f32::INFINITY,
            },
        ],
        profiles: vec![
            ProfileDef {
                move_class: 0,
                size_class: 0,
            },
            ProfileDef {
                move_class: 1,
                size_class: 3,
            },
        ],
        cluster_cells: CLUSTER,
        areas: Vec::new(),
    });
    assert_eq!(s.size_for_radius(0.3), 0);
    assert_eq!(s.size_for_radius(1.5), 1);
    assert_eq!(s.size_for_radius(6.0), 2);
    assert_eq!(s.size_for_radius(40.0), 3);
    let mut grid = NavGrid::new(Arc::new(NavStatic::build(None, s.clone()).expect("canvas"))).expect("grid");
    assert_eq!(grid.blockers.len(), 4);
    assert_eq!(grid.labels.len(), 2);
    grid.sync_stamps(vec![(1, Stamp::from_world(0.0, 0.0, 1.0))]);
    // 7 m from the disc's center: clear of the small class, inside the 1 + 9 m largest class.
    let probe = grid.cell_at(7.0, 0.0);
    assert!(grid.passable(Profile(0), probe));
    assert!(!grid.passable(Profile(1), probe));
    assert_eq!(grid.blockers[1][probe as usize], 0, "1 + 2 m does not reach 7 m");
    assert_eq!(grid.blockers[2][probe as usize], 0, "1 + 5 m does not reach 7 m");
    assert_eq!(grid.blockers[3][probe as usize], 1, "1 + 9 m does");
    // A profile index past the end is the last profile.
    assert_eq!(grid.passable(Profile(9), probe), grid.passable(Profile(1), probe));
    assert_eq!(grid.label(Profile(9), probe), grid.label(Profile(1), probe));
}

#[test]
fn settings_validation_refuses_bad_settings() {
    let good = GridSettings::hover_ground_jet();
    assert_eq!(good.validate(), Ok(()));
    let bad = |f: &dyn Fn(&mut GridSettings)| {
        let mut s = good.clone();
        f(&mut s);
        NavStatic::build(None, Arc::new(s)).err()
    };
    assert_eq!(bad(&|s| s.cell_m = 0.0), Some(GridError::BadCellSize(0.0)));
    assert!(matches!(
        bad(&|s| s.cell_m = f32::NAN),
        Some(GridError::BadCellSize(_))
    ));
    assert_eq!(
        bad(&|s| s.canvas_cells = [1, 9]),
        Some(GridError::CanvasTooSmall([1, 9]))
    );
    assert_eq!(
        bad(&|s| s.move_classes.clear()),
        Some(GridError::EmptyList("move_classes"))
    );
    assert_eq!(
        bad(&|s| s.size_classes.clear()),
        Some(GridError::EmptyList("size_classes"))
    );
    assert_eq!(
        bad(&|s| s.profiles.clear()),
        Some(GridError::EmptyList("profiles"))
    );
    assert_eq!(
        bad(&|s| s.move_classes[1] = MoveClass::new(0.5, 0.5, 1.0)),
        Some(GridError::BadMoveClass { index: 1 })
    );
    assert_eq!(
        bad(&|s| s.size_classes[2].inflate_m = -1.0),
        Some(GridError::BadSizeClass { index: 2 })
    );
    assert_eq!(
        bad(&|s| s.profiles[4].size_class = 3),
        Some(GridError::ProfileClassMissing {
            index: 4,
            move_class: 2,
            size_class: 3
        })
    );
    assert!(matches!(
        bad(&|s| s.profiles = vec![s.profiles[0]; 256]),
        Some(GridError::TooManyProfiles(256))
    ));
    let one_row = HeightCells::centered(8, 1, 5.0, vec![0.0; 8]);
    assert!(matches!(
        NavStatic::build(Some(&one_row), Arc::new(good.clone())),
        Err(GridError::GroundTooSmall { cols: 8, rows: 1, .. })
    ));
    let stat = Arc::new(NavStatic::build(None, Arc::new(good)).expect("canvas"));
    assert_eq!(
        NavGrid::with_closed_cells(stat, vec![0; 3]).err(),
        Some(GridError::ClosedMaskLength {
            got: 3,
            cells: 256 * 256
        })
    );
}

// ------------------------------------------------------------------ masks and queries

#[test]
fn an_exclusion_disc_closes_the_pit_floor_for_every_profile() {
    // A pit: flat rim at 0 m, a -100 m floor inside 20 m, a steep wall between.
    let pit = terrain(|x, z| {
        let r = regia_detsim::math::sqrt(x * x + z * z);
        if r >= 30.0 {
            0.0
        } else if r <= 20.0 {
            -100.0
        } else {
            -100.0 + (r - 20.0) * 10.0
        }
    });
    let mut grid = build(&pit);
    let floor = grid.cell_at(0.0, 0.0);
    assert!(grid.passable(Profile::JET, floor), "the floor itself is flat");
    let before = grid.state_hash();
    let disc = ExclusionDisc {
        center_x: 0.0,
        center_z: 0.0,
        radius: 25.0,
        below_y: -25.0,
    };
    grid.set_exclusions(&[disc]);
    for p in grid.settings().all_profiles() {
        assert!(!grid.passable(p, floor), "{p:?} kept off the pit floor");
        assert_eq!(grid.label(p, floor), BLOCKED);
    }
    assert!(grid.excluded(floor));
    let rim = grid.cell_at(40.0, 0.0);
    assert!(
        !grid.excluded(rim) && grid.passable(Profile::HOVER, rim),
        "the rim stays open"
    );
    let after = grid.state_hash();
    grid.set_exclusions(&[disc]);
    assert_eq!(grid.state_hash(), after, "the same discs are a no-op");
    grid.set_exclusions(&[]);
    assert_eq!(grid.state_hash(), before, "clearing restores the grid");
}

#[test]
fn closed_cells_block_every_profile_and_split_regions() {
    let stat = Arc::new(NavStatic::build(None, settings()).expect("canvas"));
    let n = stat.width * stat.height;
    let mut closed = vec![0u8; n];
    // A wall across cluster (2, 2): local column 3 closed on all 8 rows.
    for z in 16..24 {
        closed[z * stat.width + 19] = 1;
    }
    let grid = NavGrid::with_closed_cells(stat, closed).expect("grid");
    let wall = grid.index(19, 20);
    for p in grid.settings().all_profiles() {
        assert!(!grid.passable(p, wall));
        assert_eq!(grid.cost(p, wall), 0);
    }
    let cluster = grid.cluster_of(wall);
    assert_eq!(
        grid.regions[0][cluster].reps.len(),
        2,
        "the wall splits the cluster in two"
    );
    assert_ne!(
        grid.label(Profile::HOVER, grid.index(17, 20)),
        grid.label(Profile::HOVER, grid.index(21, 20))
    );
    assert_eq!(grid.summary().closed_cells, 8);
}

#[test]
fn nearest_passable_and_line_of_sight_respect_stamps() {
    let mut grid = NavGrid::new(Arc::new(NavStatic::build(None, settings()).expect("canvas"))).expect("grid");
    grid.sync_stamps(vec![(1, Stamp::from_world(0.0, 0.0, 6.0))]);
    let inside = grid.cell_at(0.0, 0.0);
    assert!(!grid.passable(Profile::HOVER, inside));
    let snapped = grid
        .nearest_passable(Profile::HOVER, inside, 8)
        .expect("a free cell near");
    assert!(grid.passable(Profile::HOVER, snapped));
    let [sx, sz] = grid.center(snapped);
    let d2 = sx * sx + sz * sz;
    assert!(d2 >= 10.5 * 10.5, "outside the 6 + 4.5 m inflated disc: {d2}");
    assert!(
        grid.nearest_passable(Profile::HOVER, inside, 1).is_none(),
        "one ring is not enough"
    );
    // A line through the disc is blocked; one beside it is clear; `ignore` lets a target inside.
    let (west, east) = (grid.cell_at(-40.0, 0.0), grid.cell_at(40.0, 0.0));
    assert!(!grid.los(Profile::HOVER, west, east, COST_FACE, 0));
    let (nw, ne) = (grid.cell_at(-40.0, 30.0), grid.cell_at(40.0, 30.0));
    assert!(grid.los(Profile::HOVER, nw, ne, COST_FACE, 0));
    assert!(grid.los(Profile::HOVER, west, inside, COST_FACE, 3));
}

#[test]
fn a_cloned_grid_keeps_its_state_and_diverges_on_its_own() {
    let ground = rough_ground();
    let mut a = build(&ground);
    a.sync_stamps(api_stamps(&scenario_ticks()[0]));
    let b = a.clone();
    assert_eq!(a.state_hash(), b.state_hash());
    assert!(Arc::ptr_eq(&a.stat, &b.stat), "the terrain half is shared");
    a.sync_stamps(Vec::new());
    assert_ne!(a.state_hash(), b.state_hash());
}

#[test]
fn summary_serializes_with_hashes_and_counts() {
    let mut grid = build(&rough_ground());
    grid.sync_stamps(vec![(3, Stamp::from_world(10.0, -20.0, 5.0))]);
    let summary = grid.summary();
    assert_eq!(summary.width, 96);
    assert_eq!(summary.height, 80);
    assert_eq!(summary.clusters, [12, 10]);
    assert_eq!(summary.stamps, 1);
    assert!(!summary.flat);
    assert_eq!(summary.profiles.len(), 5);
    assert!(
        summary
            .profiles
            .iter()
            .all(|p| p.passable_cells > 0 && p.regions > 0)
    );
    assert_eq!(summary.static_hash, format!("{:016x}", grid.stat.bits_hash()));
    let text = ron::to_string(&summary).expect("serializes");
    assert!(text.contains("state_hash"), "{text}");
}

// ------------------------------------------------------------------ bits and rules

/// The grid's bits on a fixed input, pinned by hash: the same values must come out on x86-64 and
/// aarch64 (ADR-8012 A-4). A change here moves a lockstep sim's grid: a sim-version change.
#[test]
fn grid_bits_match_the_pinned_hashes() {
    let mut grid = build(&rough_ground());
    let static_hash = grid.stat.bits_hash();
    let mut states = Vec::new();
    for ents in scenario_ticks().iter().step_by(2) {
        grid.sync_stamps(api_stamps(ents));
        states.push(grid.state_hash());
    }
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for s in &states {
        fold = (fold ^ *s).wrapping_mul(0x100_0000_01b3);
    }
    println!("static {static_hash:#018x} states {fold:#018x}");
    assert_eq!(static_hash, PINNED_STATIC, "terrain half moved");
    assert_eq!(fold, PINNED_STATES, "live state moved");
}

/// Recorded on x86-64 Linux (the build server, Rust 1.99) on 2026-11-16.
const PINNED_STATIC: u64 = 0xb9e5_02c2_b132_cdaf;
/// Recorded with [`PINNED_STATIC`].
const PINNED_STATES: u64 = 0x098b_1a72_c0a4_f8ed;

/// The deterministic navigation crate stays free of the ECS navigation crate (no ECS, scene or GPU) so a
/// lockstep sim can link it alone; its only imports are std, serde, tracing and the lockstep kit.
/// ADR-8877: the module became the `regia_nav_det` crate, so the scan covers its whole `src/`, and
/// `crate::agent` is now this crate's deterministic agents: the ban on `regia_nav`'s ECS agents is a
/// ban on naming `regia_nav` at all (its normal dependency closure is checked by `xtask`'s
/// `regia_nav_det_depends_on_no_ecs_crate`).
#[test]
fn det_module_imports_only_std_serde_tracing_and_the_lockstep_kit() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)
            .expect("src dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                files.push(p);
            }
        }
    }
    assert!(files.len() >= 3, "{files:?}");
    // ADR-8195: the tunnel layer reads the tunnel description of `regia_world_geom::compat`.
    let allowed = [
        "std::",
        "serde::",
        "regia_detsim::",
        "regia_world_geom::compat::",
        "super::",
        "crate::",
    ];
    for file in files {
        let text = std::fs::read_to_string(&file).expect("source");
        for (n, line) in text.lines().enumerate() {
            let t = line.trim_start();
            let Some(path) = t.strip_prefix("use ") else {
                continue;
            };
            assert!(
                allowed.iter().any(|a| path.starts_with(a)),
                "{}:{}: `{t}` leaves the deterministic module",
                file.display(),
                n + 1
            );
        }
        // Built from pieces so this line does not name them itself.
        for banned in ["ecs", "app", "scene", "core"].map(|c| ["regia_", c].concat()) {
            let hit = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(banned.as_str()));
            assert!(!hit, "{}: names `{banned}`", file.display());
        }
        for banned in [["be", "vy"].concat(), ["regia_", "nav::"].concat()] {
            assert!(!text.contains(&banned), "{}: names `{banned}`", file.display());
        }
    }
}

// ------------------------------------------------------------------ cluster size (ADR-8295)

/// ADR-8295: the default settings keep DO27's 8 x 8 clusters and one-byte labels, and the grid
/// pins of `grid_bits_match_the_pinned_hashes` come out of settings that name the cluster
/// explicitly. Settings written before the field existed load with 8.
#[test]
fn default_cluster_keeps_the_pins() {
    let s = GridSettings::default();
    assert_eq!(s.cluster_cells, CLUSTER);
    assert_eq!(s.cluster_cells, 8);
    assert_eq!(s.label_width(), LabelWidth::U8);
    let explicit = Arc::new(GridSettings {
        cluster_cells: 8,
        ..GridSettings::hover_ground_jet()
    });
    let mut grid = NavGrid::new(Arc::new(
        NavStatic::build(Some(&rough_ground()), explicit).expect("static"),
    ))
    .expect("grid");
    assert_eq!(grid.cluster_cells(), CLUSTER);
    assert_eq!(grid.max_regions(), MAX_REGIONS);
    assert_eq!(grid.label_width(), LabelWidth::U8);
    assert!(grid.wide_labels.is_empty());
    let static_hash = grid.stat.bits_hash();
    let mut states = Vec::new();
    for ents in scenario_ticks().iter().step_by(2) {
        grid.sync_stamps(api_stamps(ents));
        states.push(grid.state_hash());
    }
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for s in &states {
        fold = (fold ^ *s).wrapping_mul(0x100_0000_01b3);
    }
    assert_eq!(static_hash, PINNED_STATIC, "terrain half moved");
    assert_eq!(fold, PINNED_STATES, "live state moved");
    // Old settings text (no `cluster_cells`) loads as the default cluster.
    let mut text = ron::to_string(&GridSettings::hover_ground_jet()).expect("ron");
    text = text.replace(",cluster_cells:8", "");
    assert!(!text.contains("cluster_cells"), "{text}");
    let old: GridSettings = ron::from_str(&text).expect("old settings");
    assert_eq!(old, GridSettings::hover_ground_jet());
}

/// ADR-8295: the cluster size is a power of two from 2 to 128; 16 and above store two-byte labels,
/// and a cluster of 16 or 32 labels the same regions as the flood at one byte would (every open
/// cell of a cluster in exactly one region, regions 4-connected).
#[test]
fn cluster_size_is_a_setting_and_wide_clusters_label_two_bytes() {
    for bad in [0usize, 1, 3, 12, 24, 256] {
        let s = GridSettings {
            cluster_cells: bad,
            ..GridSettings::hover_ground_jet()
        };
        assert_eq!(s.validate(), Err(GridError::BadClusterSize(bad)), "{bad}");
    }
    for (k, width) in [
        (2usize, LabelWidth::U8),
        (4, LabelWidth::U8),
        (8, LabelWidth::U8),
        (16, LabelWidth::U16),
        (32, LabelWidth::U16),
        (128, LabelWidth::U16),
    ] {
        let s = GridSettings {
            cluster_cells: k,
            ..GridSettings::hover_ground_jet()
        };
        assert_eq!(s.validate(), Ok(()), "{k}");
        assert_eq!(s.label_width(), width, "{k}");
    }
    // A checkerboard of closed cells: every open cell is its own region, the worst case for the
    // label type (k^2 / 2 regions per cluster: 128 at 16, 512 at 32).
    for k in [16usize, 32] {
        let s = Arc::new(GridSettings {
            cluster_cells: k,
            canvas_cells: [64, 64],
            ..GridSettings::hover_ground_jet()
        });
        let stat = Arc::new(NavStatic::build(None, s).expect("static"));
        let closed: Vec<u8> = (0..64 * 64).map(|i| ((i % 64 + i / 64) % 2) as u8).collect();
        let grid = NavGrid::with_closed_cells(stat, closed).expect("grid");
        assert_eq!(grid.label_width(), LabelWidth::U16);
        assert!(grid.labels.iter().all(|l| l.is_empty()));
        assert_eq!(grid.clusters_x, 64 / k);
        let p = Profile::FOOT;
        let pi = grid.settings().profile_index(p);
        for c in 0..grid.clusters_x * grid.clusters_z {
            assert_eq!(grid.regions[pi][c].reps.len(), k * k / 2, "cluster {c} at {k}");
        }
        let cell = grid.index(k + 2, 2);
        assert!(grid.passable(p, cell));
        assert!(grid.region_label(p, cell) < (k * k / 2) as u16);
        assert_eq!(grid.region_label(p, grid.index(0, 1)), NO_REGION);
        assert_eq!(grid.label(p, grid.index(0, 1)), BLOCKED);
        assert_eq!(grid.cluster_of(cell), 1);
    }
    // Open ground at 32: one region per cluster, the same passable cells as at 8.
    let open = |k: usize| {
        let s = Arc::new(GridSettings {
            cluster_cells: k,
            ..GridSettings::hover_ground_jet()
        });
        let mut g = NavGrid::new(Arc::new(
            NavStatic::build(Some(&rough_ground()), s).expect("static"),
        ))
        .expect("grid");
        g.sync_stamps(api_stamps(&scenario_ticks()[3]));
        g
    };
    let (a, b) = (open(8), open(32));
    for p in a.settings().all_profiles() {
        for c in 0..(a.width() * a.height()) as u32 {
            assert_eq!(a.passable(p, c), b.passable(p, c));
            assert_eq!(
                a.region_label(p, c) == NO_REGION,
                b.region_label(p, c) == NO_REGION
            );
        }
    }
    assert!(b.generation() > 0);
    assert_ne!(
        a.state_hash(),
        b.state_hash(),
        "two-byte labels hash their own bytes"
    );
}

// ------------------------------------------------------------------ areas, overlays, shapes (GR-6/7/8)

/// A flat grid of `cols x rows` cells of `cell_m` with these settings changes.
fn flat_grid(cols: usize, rows: usize, cell_m: f32, f: impl Fn(&mut GridSettings)) -> NavGrid {
    let mut s = GridSettings::three_class();
    s.canvas_cells = [cols, rows];
    s.cell_m = cell_m;
    f(&mut s);
    let stat = NavStatic::build(None, Arc::new(s)).expect("valid settings");
    NavGrid::new(Arc::new(stat)).expect("grid")
}

fn road_settings(s: &mut GridSettings) {
    s.areas.push(AreaDef {
        name: "road".into(),
        cost_pct: 50,
        blocked_for: ProfileMask::NONE,
    });
    s.areas.push(AreaDef {
        name: "deep_water".into(),
        cost_pct: 300,
        blocked_for: ProfileMask::of(&[Profile::MID_CLIMB_SMALL, Profile::MID_CLIMB_MEDIUM]),
    });
}

/// Sum of `cost` over a hand path of cell coordinates.
fn path_cost(grid: &NavGrid, profile: Profile, cells: &[(usize, usize)]) -> u32 {
    cells
        .iter()
        .map(|&(x, z)| grid.cost(profile, grid.index(x, z)))
        .sum()
}

#[test]
fn default_areas_and_no_overlay_keep_the_pins() {
    // The neutral preset is the old one bit for bit.
    assert_eq!(GridSettings::three_class(), GridSettings::hover_ground_jet());
    assert_eq!(GridSettings::default(), GridSettings::hover_ground_jet());
    assert_eq!(MoveClass::CLIMB_LOW, MoveClass::HOVER);
    assert_eq!(MoveClass::CLIMB_MID, MoveClass::GROUND);
    assert_eq!(MoveClass::CLIMB_HIGH, MoveClass::JET);
    assert_eq!(
        [
            Profile::LOW_CLIMB_MEDIUM,
            Profile::LOW_CLIMB_LARGE,
            Profile::MID_CLIMB_SMALL,
            Profile::MID_CLIMB_MEDIUM,
            Profile::HIGH_CLIMB_SMALL
        ],
        [
            Profile::HOVER,
            Profile::HOVER_LARGE,
            Profile::FOOT,
            Profile::WALKER,
            Profile::JET
        ]
    );
    let stat = NavStatic::build(Some(&rough_ground()), Arc::new(GridSettings::three_class())).expect("valid");
    let mut grid = NavGrid::new(Arc::new(stat)).expect("grid");
    // Area 0 everywhere, no overlay, no shapes: nothing moves.
    let all = PaintRegion::CellRect {
        min: [0, 0],
        max: [u32::MAX, u32::MAX],
    };
    assert_eq!(grid.paint_area(&all, 0), Ok(0));
    assert_eq!(grid.set_overlay(None), Ok(()));
    assert!(!grid.sync_shapes_from(&mut []));
    let static_hash = grid.stat.bits_hash();
    let mut states = Vec::new();
    let mut buffer = Vec::new();
    for ents in scenario_ticks().iter().step_by(2) {
        buffer.clear();
        buffer.extend(api_stamps(ents));
        grid.sync_stamps_from(&mut buffer);
        states.push(grid.state_hash());
        // The cost every search reads is the terrain cost wherever the cell is open.
        for p in grid.settings().all_profiles() {
            let (m, _) = grid.classes_of(p);
            for c in 0..(grid.width() * grid.height()) as u32 {
                let want = if grid.passable(p, c) {
                    u32::from(grid.stat.cost[m][c as usize])
                } else {
                    0
                };
                assert_eq!(grid.cost(p, c), want, "profile {p:?} cell {c}");
            }
        }
    }
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for s in &states {
        fold = (fold ^ *s).wrapping_mul(0x100_0000_01b3);
    }
    assert_eq!(static_hash, PINNED_STATIC, "terrain half moved");
    assert_eq!(fold, PINNED_STATES, "live state moved");

    // A road painted and painted back leaves the same bits as a grid that never had one.
    let mut s = GridSettings::three_class();
    road_settings(&mut s);
    let stat = NavStatic::build(Some(&rough_ground()), Arc::new(s)).expect("valid");
    let mut painted = NavGrid::new(Arc::new(stat)).expect("grid");
    let plain = painted.state_hash();
    let strip = PaintRegion::CellRect {
        min: [3, 10],
        max: [60, 12],
    };
    assert_eq!(painted.paint_area(&strip, 1), Ok(58 * 3));
    assert_ne!(painted.state_hash(), plain);
    assert_eq!(painted.paint_area(&strip, 0), Ok(58 * 3));
    assert_eq!(painted.state_hash(), plain);
    assert_eq!(painted.painted_cells(), 0);
}

#[test]
fn a_road_area_halves_the_route_cost() {
    let mut grid = flat_grid(64, 64, 5.0, road_settings);
    let road = grid.settings().area_id("road").expect("road area");
    assert_eq!(road, 1);
    assert_eq!(grid.settings().area_id(DEFAULT_AREA_NAME), Some(0));
    assert_eq!(grid.settings().min_area_pct(), 50);
    let before = grid.generation();
    let painted = grid
        .paint_area(
            &PaintRegion::CellRect {
                min: [0, 20],
                max: [63, 20],
            },
            road,
        )
        .expect("paint");
    assert_eq!(painted, 64);
    assert!(
        grid.generation() > before,
        "painting relabels and bumps the generation"
    );
    let on_road: Vec<(usize, usize)> = (5..=50).map(|x| (x, 20)).collect();
    let off_road: Vec<(usize, usize)> = (5..=50).map(|x| (x, 30)).collect();
    for p in grid.settings().all_profiles() {
        let a = path_cost(&grid, p, &on_road);
        let b = path_cost(&grid, p, &off_road);
        assert_eq!(b, 46 * COST_FLAT, "profile {p:?}");
        assert_eq!(a * 2, b, "profile {p:?}: the road halves the route cost");
        // A detour down to the road and back beats the straight line 4 rows off it.
        let straight: Vec<(usize, usize)> = (5..=50).map(|x| (x, 24)).collect();
        let mut detour: Vec<(usize, usize)> = (21..24).rev().map(|z| (5, z)).collect();
        detour.extend(on_road.iter().copied());
        detour.extend((21..=24).map(|z| (50, z)));
        assert!(path_cost(&grid, p, &detour) < path_cost(&grid, p, &straight));
    }
    // The coarse regions see the road too: its cluster row is cheaper.
    let pi = grid.settings().profile_index(Profile::LOW_CLIMB_MEDIUM);
    let on = grid.regions[pi][grid.cluster_of(grid.index(10, 20))].costs[0];
    let off = grid.regions[pi][grid.cluster_of(grid.index(10, 30))].costs[0];
    assert_eq!(off, 10);
    assert!(on < off, "road cluster mean {on} vs {off}");
    // A polygon paint: deep water (300 %, blocked for the mid-climb profiles).
    let water = grid.settings().area_id("deep_water").expect("water");
    let quad = vec![[-2000, -2000], [2000, -2000], [2000, 2000], [-2000, 2000]];
    let n = grid
        .paint_area(&PaintRegion::Polygon(quad), water)
        .expect("paint");
    // 40 m square, boundary inclusive, 5 m cells: 9 x 9 centers.
    assert_eq!(n, 81);
    let c = grid.cell_at(0.0, 0.0);
    assert_eq!(grid.area_of(c), water);
    assert_eq!(grid.cost(Profile::LOW_CLIMB_MEDIUM, c), 30);
    assert_eq!(grid.cost(Profile::MID_CLIMB_SMALL, c), 0);
    assert!(!grid.passable(Profile::MID_CLIMB_SMALL, c));
    assert!(!grid.passable(Profile::MID_CLIMB_MEDIUM, c));
    assert!(grid.passable(Profile::HIGH_CLIMB_SMALL, c));
    assert_eq!(grid.label(Profile::MID_CLIMB_SMALL, c), BLOCKED);
    assert_eq!(grid.summary().painted_cells, 64 + 81);
}

#[test]
fn area_cost_rounding_is_written_down() {
    // max(1, (terrain * pct + 50) / 100) on flat ground (terrain 10).
    for (pct, want) in [
        (1u16, 1u32),
        (4, 1),
        (5, 1),
        (14, 1),
        (15, 2),
        (33, 3),
        (50, 5),
        (100, 10),
        (155, 16),
        (300, 30),
        (u16::MAX, 6554),
    ] {
        let mut grid = flat_grid(16, 16, 5.0, |s| {
            s.areas.push(AreaDef {
                name: "a".into(),
                cost_pct: pct,
                blocked_for: ProfileMask::NONE,
            })
        });
        let rect = PaintRegion::CellRect {
            min: [0, 0],
            max: [15, 15],
        };
        grid.paint_area(&rect, 1).expect("paint");
        assert_eq!(grid.cost(Profile::LOW_CLIMB_MEDIUM, 17), want, "pct {pct}");
        // Region costs follow (one open cluster: the mean is the cell cost).
        let pi = grid.settings().profile_index(Profile::LOW_CLIMB_MEDIUM);
        assert_eq!(u32::from(grid.regions[pi][0].costs[0]), want, "pct {pct}");
    }
    // On sloped ground: terrain 250 at 1 % is 3, a cliff stays a cliff.
    let ground = terrain(|x, _| if x > 0.0 { x * 0.7 } else { 0.0 });
    let mut s = GridSettings::three_class();
    s.areas.push(AreaDef {
        name: "a".into(),
        cost_pct: 1,
        blocked_for: ProfileMask::NONE,
    });
    let stat = NavStatic::build(Some(&ground), Arc::new(s)).expect("valid");
    let mut grid = NavGrid::new(Arc::new(stat)).expect("grid");
    let all = PaintRegion::CellRect {
        min: [0, 0],
        max: [u32::MAX, u32::MAX],
    };
    grid.paint_area(&all, 1).expect("paint");
    for c in 0..(grid.width() * grid.height()) as u32 {
        for p in grid.settings().all_profiles() {
            let (m, _) = grid.classes_of(p);
            let t = u32::from(grid.stat.cost[m][c as usize]);
            let want = if t == 0 { 0 } else { ((t + 50) / 100).max(1) };
            assert_eq!(grid.cost(p, c), want);
        }
    }
}

#[test]
fn bad_areas_are_refused() {
    let area = |name: &str, pct: u16| AreaDef {
        name: name.into(),
        cost_pct: pct,
        blocked_for: ProfileMask::NONE,
    };
    let build = |areas: Vec<AreaDef>| {
        let mut s = GridSettings::three_class();
        s.areas = areas;
        NavStatic::build(None, Arc::new(s)).err()
    };
    assert_eq!(
        build(vec![area("road", 50), area("mud", 0)]),
        Some(GridError::BadAreaCost { index: 1 })
    );
    assert_eq!(
        build(vec![area("road", 50), area("road", 70)]),
        Some(GridError::DuplicateAreaName("road".into()))
    );
    assert_eq!(
        build(vec![area(DEFAULT_AREA_NAME, 70)]),
        Some(GridError::DuplicateAreaName(DEFAULT_AREA_NAME.into()))
    );
    let many: Vec<AreaDef> = (0..256).map(|k| area(&format!("a{k}"), 100)).collect();
    assert_eq!(build(many), Some(GridError::TooManyAreas(256)));
    let most: Vec<AreaDef> = (0..255).map(|k| area(&format!("a{k}"), 100)).collect();
    assert_eq!(build(most), None, "255 areas fit a byte");
    // Painting an id the settings do not define, or a polygon without area.
    let mut grid = flat_grid(16, 16, 5.0, road_settings);
    let rect = PaintRegion::CellRect {
        min: [0, 0],
        max: [3, 3],
    };
    assert_eq!(grid.paint_area(&rect, 3), Err(GridError::AreaMissing(3)));
    let line = PaintRegion::Polygon(vec![[0, 0], [500, 500], [1000, 1000]]);
    assert_eq!(grid.paint_area(&line, 1), Err(GridError::BadPolygon(3)));
    assert_eq!(
        grid.paint_area(&PaintRegion::Polygon(vec![[0, 0], [500, 0]]), 1),
        Err(GridError::BadPolygon(2))
    );
    // Settings round-trip through RON with and without areas (old files have none).
    let mut s = GridSettings::three_class();
    road_settings(&mut s);
    let text = ron::to_string(&s).expect("serializes");
    let back: GridSettings = ron::from_str(&text).expect("parses");
    assert_eq!(back, s);
    let old = ron::to_string(&GridSettings::three_class())
        .expect("serializes")
        .replace(",areas:[]", "");
    assert!(!old.contains("areas"), "{old}");
    let back: GridSettings = ron::from_str(&old).expect("an old file parses");
    assert_eq!(back, GridSettings::three_class());
}

#[test]
fn overlay_changes_cost_and_bumps_the_generation() {
    let mut grid = build(&rough_ground());
    grid.sync_stamps(api_stamps(&scenario_ticks()[0]));
    let plain_hash = grid.state_hash();
    let plain_regions = grid.regions.clone();
    let plain_costs: Vec<Vec<u32>> = grid
        .settings()
        .all_profiles()
        .map(|p| {
            (0..(grid.width() * grid.height()) as u32)
                .map(|c| grid.cost(p, c))
                .collect()
        })
        .collect();
    // A threat disc of 60 m around (-100, 0), paid by the low-climb medium and the small walker.
    let n = grid.width() * grid.height();
    let mut cells = vec![0u16; n];
    for (c, v) in cells.iter_mut().enumerate() {
        let [x, z] = grid.center(c as u32);
        let (dx, dz) = (x + 100.0, z);
        if dx * dx + dz * dz < 3600.0 {
            *v = 200;
        }
    }
    let threat = Arc::new(CostOverlay {
        profiles: ProfileMask::of(&[Profile::LOW_CLIMB_MEDIUM, Profile::MID_CLIMB_SMALL]),
        cells,
    });
    let gen0 = grid.generation();
    grid.set_overlay(Some(Arc::clone(&threat))).expect("overlay");
    let gen1 = grid.generation();
    assert!(gen1 > gen0, "a new overlay bumps the generation");
    for (pi, p) in grid.settings().all_profiles().enumerate() {
        for c in 0..n as u32 {
            let before = plain_costs[pi][c as usize];
            let extra = if threat.profiles.contains(p) {
                u32::from(threat.cells[c as usize])
            } else {
                0
            };
            // Blocked cells stay blocked; open ones pay the overlay.
            let want = if before == 0 { 0 } else { before + extra };
            assert_eq!(grid.cost(p, c), want, "profile {p:?} cell {c}");
        }
    }
    let center = grid.cell_at(-100.0, 0.0);
    let pi = grid.settings().profile_index(Profile::LOW_CLIMB_MEDIUM);
    let cl = grid.cluster_of(center);
    assert!(
        grid.regions[pi][cl].costs.iter().all(|c| *c >= 200),
        "region costs include the overlay"
    );
    let walker = grid.settings().profile_index(Profile::MID_CLIMB_MEDIUM);
    assert_eq!(
        grid.regions[walker][cl], plain_regions[walker][cl],
        "unmasked profiles keep theirs"
    );
    assert_ne!(grid.state_hash(), plain_hash);
    assert!(grid.summary().overlay);
    // The same overlay again is a no-op; a wrong length is refused.
    grid.set_overlay(Some(Arc::clone(&threat))).expect("overlay");
    assert_eq!(grid.generation(), gen1);
    let short = Arc::new(CostOverlay {
        profiles: ProfileMask::ALL,
        cells: vec![0; 3],
    });
    assert_eq!(
        grid.set_overlay(Some(short)),
        Err(GridError::OverlayLength { got: 3, cells: n })
    );
    // Clearing it restores every bit.
    grid.set_overlay(None).expect("clear");
    assert!(grid.generation() > gen1);
    assert_eq!(grid.state_hash(), plain_hash);
    assert_eq!(grid.regions, plain_regions);
}

/// Exact distance test by a second formula: the closest point on `a..b` scaled by `len2`, compared
/// in `len2^2` units.
fn brute_segment_closer(a: [i64; 2], b: [i64; 2], p: [i64; 2], r: i64) -> bool {
    let (ex, ez) = (i128::from(b[0] - a[0]), i128::from(b[1] - a[1]));
    let len2 = ex * ex + ez * ez;
    let (px, pz) = (i128::from(p[0]), i128::from(p[1]));
    let r = i128::from(r);
    if len2 == 0 {
        let (dx, dz) = (px - i128::from(a[0]), pz - i128::from(a[1]));
        return dx * dx + dz * dz < r * r;
    }
    let t = ((px - i128::from(a[0])) * ex + (pz - i128::from(a[1])) * ez).clamp(0, len2);
    // Closest point times len2.
    let (qx, qz) = (i128::from(a[0]) * len2 + t * ex, i128::from(a[1]) * len2 + t * ez);
    let (dx, dz) = (px * len2 - qx, pz * len2 - qz);
    dx * dx + dz * dz < r * r * len2 * len2
}

/// Strictly inside by ray crossing (points on the boundary are not inside).
fn brute_inside(poly: &[[i64; 2]], p: [i64; 2]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let on_edge = (0..n).any(|k| {
        brute_segment_closer(poly[k], poly[(k + 1) % n], p, 1) && {
            // Within 1 cm: check exactly that it is on the segment.
            let (a, b) = (poly[k], poly[(k + 1) % n]);
            let cross = i128::from(b[0] - a[0]) * i128::from(p[1] - a[1])
                - i128::from(b[1] - a[1]) * i128::from(p[0] - a[0]);
            cross == 0
                && p[0] >= a[0].min(b[0])
                && p[0] <= a[0].max(b[0])
                && p[1] >= a[1].min(b[1])
                && p[1] <= a[1].max(b[1])
        }
    });
    if on_edge {
        return false;
    }
    let mut inside = false;
    for k in 0..n {
        let (a, b) = (poly[k], poly[(k + 1) % n]);
        if (a[1] > p[1]) != (b[1] > p[1]) {
            // x of the crossing compared without division: sign-aware.
            let lhs = i128::from(p[0] - a[0]) * i128::from(b[1] - a[1]);
            let rhs = i128::from(b[0] - a[0]) * i128::from(p[1] - a[1]);
            let left = if b[1] > a[1] { lhs < rhs } else { lhs > rhs };
            if left {
                inside = !inside;
            }
        }
    }
    inside
}

/// The test's own copy of the documented corner rule.
fn brute_corners(shape: &StampShape) -> Vec<[i64; 2]> {
    match shape {
        StampShape::Square { x_cm, z_cm, half_cm } => {
            let (x, z, h) = (i64::from(*x_cm), i64::from(*z_cm), i64::from(*half_cm));
            vec![[x - h, z - h], [x + h, z - h], [x + h, z + h], [x - h, z + h]]
        }
        StampShape::OrientedBox {
            x_cm,
            z_cm,
            half_x_cm,
            half_z_cm,
            cos_sin_q,
        } => {
            let (c, s) = (f64::from(cos_sin_q[0]), f64::from(cos_sin_q[1]));
            let (hx, hz) = (f64::from(*half_x_cm), f64::from(*half_z_cm));
            [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
                .iter()
                .map(|(sx, sz)| {
                    // n / 16384 is exact in f64; `round` is half away from zero.
                    let wx = ((sx * hx * c - sz * hz * s) / 16384.0).round() as i64;
                    let wz = ((sx * hx * s + sz * hz * c) / 16384.0).round() as i64;
                    [i64::from(*x_cm) + wx, i64::from(*z_cm) + wz]
                })
                .collect()
        }
        StampShape::ConvexPolygon(points) => points
            .iter()
            .map(|p| [i64::from(p[0]), i64::from(p[1])])
            .collect(),
        StampShape::Disc(_) => Vec::new(),
    }
}

/// Convex hull (monotone chain) of integer points, counter-clockwise.
fn hull(mut pts: Vec<[i64; 2]>) -> Vec<[i64; 2]> {
    pts.sort_unstable();
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross =
        |o: [i64; 2], a: [i64; 2], b: [i64; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let mut lower: Vec<[i64; 2]> = Vec::new();
    for p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], *p) <= 0 {
            lower.pop();
        }
        lower.push(*p);
    }
    let mut upper: Vec<[i64; 2]> = Vec::new();
    for p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], *p) <= 0 {
            upper.pop();
        }
        upper.push(*p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Seeded boxes, squares and polygons (some past the map edge, some degenerate).
fn seeded_shapes() -> Vec<StampShape> {
    let mut rng = Lcg(0x5eed_5a9e);
    let mut shapes = Vec::new();
    for k in 0..24 {
        let (x, z) = (
            rng.range(-26000.0, 26000.0) as i32,
            rng.range(-22000.0, 22000.0) as i32,
        );
        match k % 4 {
            0 => shapes.push(StampShape::oriented_box(
                x as f32 / 100.0,
                z as f32 / 100.0,
                rng.range(0.5, 30.0),
                rng.range(0.5, 12.0),
                rng.range(-1.0, 1.0),
                rng.range(-1.0, 1.0),
            )),
            1 => shapes.push(StampShape::Square {
                x_cm: x,
                z_cm: z,
                half_cm: rng.range(50.0, 2500.0) as i32,
            }),
            2 => {
                let pts: Vec<[i64; 2]> = (0..7)
                    .map(|_| {
                        [
                            i64::from(x) + rng.range(-3000.0, 3000.0) as i64,
                            i64::from(z) + rng.range(-3000.0, 3000.0) as i64,
                        ]
                    })
                    .collect();
                let mut poly: Vec<[i32; 2]> = hull(pts).iter().map(|p| [p[0] as i32, p[1] as i32]).collect();
                if k % 8 == 6 {
                    poly.reverse(); // clockwise works too
                }
                shapes.push(StampShape::ConvexPolygon(poly));
            }
            _ => {
                // Exact cell-center alignment: a box whose edges pass through cell centers.
                let (cx, cz) = ((x / 500) * 500, (z / 500) * 500);
                shapes.push(StampShape::OrientedBox {
                    x_cm: cx,
                    z_cm: cz,
                    half_x_cm: 1000,
                    half_z_cm: 500,
                    cos_sin_q: [0, 16384],
                });
            }
        }
    }
    // Degenerate: a segment, a single point, collinear points (only the inflation band covers).
    shapes.push(StampShape::ConvexPolygon(vec![[-5000, 1000], [5000, 1000]]));
    shapes.push(StampShape::ConvexPolygon(vec![[1250, -1250]]));
    shapes.push(StampShape::ConvexPolygon(vec![
        [0, 0],
        [1000, 1000],
        [2000, 2000],
    ]));
    // Mostly off the map, and wholly off it.
    shapes.push(StampShape::Square {
        x_cm: 24500,
        z_cm: -20500,
        half_cm: 1500,
    });
    shapes.push(StampShape::Square {
        x_cm: 90000,
        z_cm: 90000,
        half_cm: 1500,
    });
    shapes
}

#[test]
fn oriented_box_and_polygon_stamps_equal_a_brute_force_rasterization() {
    // Four size classes: no inflation, the preset's three, and an odd centimeter count.
    let classes = |s: &mut GridSettings| {
        s.size_classes = vec![
            SizeClass {
                inflate_m: 0.0,
                max_radius_m: 0.5,
            },
            SizeClass::SMALL,
            SizeClass {
                inflate_m: 2.37,
                max_radius_m: 3.0,
            },
            SizeClass::MEDIUM,
            SizeClass::LARGE,
        ];
        s.profiles = vec![ProfileDef {
            move_class: 0,
            size_class: 0,
        }];
    };
    for cell_m in [5.0f32, 3.3] {
        let mut grid = flat_grid(96, 80, cell_m, classes);
        let n = grid.width() * grid.height();
        let ox = (grid.stat.origin_x * 100.0).round() as i64;
        let oz = (grid.stat.origin_z * 100.0).round() as i64;
        let step = (cell_m * 100.0).round() as i64;
        let mut covered_any = 0usize;
        for shape in seeded_shapes() {
            let poly = brute_corners(&shape);
            let span = grid.stamp_shape(&shape, true);
            for (size, class) in grid.settings().size_classes.clone().iter().enumerate() {
                let r = (class.inflate_m * 100.0).round() as i64;
                for i in 0..n {
                    let (x, z) = (i % grid.width(), i / grid.width());
                    let p = [ox + x as i64 * step, oz + z as i64 * step];
                    assert_eq!(grid.cell_center_cm(x, z), p);
                    let near = r > 0
                        && match poly.len() {
                            0 => false,
                            1 => brute_segment_closer(poly[0], poly[0], p, r),
                            k => (0..k).any(|e| brute_segment_closer(poly[e], poly[(e + 1) % k], p, r)),
                        };
                    let want = brute_inside(&poly, p) || near;
                    assert_eq!(
                        grid.blockers[size][i] != 0,
                        want,
                        "cell_m {cell_m} size {size} cell ({x}, {z}) shape {shape:?}"
                    );
                    if want {
                        covered_any += 1;
                        // The touched cluster range holds every covered cell.
                        let (x0, z0, x1, z1) = span.expect("a covered cell means a touched range");
                        let (cx, cz) = (x / CLUSTER, z / CLUSTER);
                        assert!(cx >= x0 && cx <= x1 && cz >= z0 && cz <= z1);
                    }
                }
            }
            grid.stamp_shape(&shape, false);
            assert!(
                grid.blockers.iter().all(|l| l.iter().all(|b| *b == 0)),
                "add then lift is clean"
            );
        }
        assert!(covered_any > 1000, "the shapes cover cells: {covered_any}");
    }
    // A disc shape is the disc stamp, bit for bit.
    let mut a = build(&rough_ground());
    let mut b = build(&rough_ground());
    let disc = Stamp::from_world(-31.0, 17.5, 6.25);
    a.sync_stamps(vec![(1, disc)]);
    assert!(b.sync_shapes_from(&mut [(1, StampShape::Disc(disc))]));
    assert_eq!(a.blockers, b.blockers);
    assert_eq!(a.labels, b.labels);
    assert_eq!(a.regions, b.regions);
    assert_eq!(a.dirty_clusters(), b.dirty_clusters());
}

#[test]
fn shape_sync_lifts_moves_and_relabels_like_a_fresh_stamp() {
    let mut rng = Lcg(0x5eed_0b0c);
    let mut grid = build(&rough_ground());
    let mut set: Vec<(u32, StampShape)> = Vec::new();
    for tick in 0..10 {
        set.clear();
        for k in 0..12u32 {
            if (k + tick) % 5 == 0 {
                continue; // comes and goes
            }
            let x = rng.range(-200.0, 200.0);
            let z = rng.range(-180.0, 180.0);
            let shape = match k % 3 {
                0 => StampShape::oriented_box(x, z, 8.0, 3.0, rng.range(-1.0, 1.0), 1.0),
                1 => StampShape::polygon_from_world(&[[x, z], [x + 12.0, z], [x + 6.0, z + 9.0]]),
                _ => StampShape::Disc(Stamp::from_world(x, z, 4.0)),
            };
            set.push((k * 3 + 1, shape));
        }
        set.reverse();
        assert!(grid.sync_shapes_from(&mut set));
        let mut fresh = build(&rough_ground());
        for (_, s) in grid.shapes() {
            fresh.stamp_shape(s, true);
        }
        for c in 0..fresh.clusters_x * fresh.clusters_z {
            fresh.relabel_cluster(c);
        }
        assert_eq!(grid.blockers, fresh.blockers, "tick {tick}");
        assert_eq!(grid.labels, fresh.labels, "tick {tick}");
        assert_eq!(grid.regions, fresh.regions, "tick {tick}");
        assert!(grid.shapes().windows(2).all(|w| w[0].0 < w[1].0), "sorted by id");
        // The same set again touches nothing.
        let generation = grid.generation();
        assert!(!grid.sync_shapes_from(&mut set));
        assert!(grid.dirty_clusters().is_empty());
        assert_eq!(grid.generation(), generation);
    }
    assert_eq!(grid.summary().shapes, grid.shapes().len());
    // Lifting everything leaves the empty grid's bits.
    assert!(grid.sync_shapes_from(&mut []));
    assert_eq!(grid.state_hash(), build(&rough_ground()).state_hash());
}

#[test]
fn sync_from_equals_sync_stamps() {
    let ground = rough_ground();
    let mut by_value = build(&ground);
    let mut by_buffer = build(&ground);
    let mut buffer = Vec::new();
    for (tick, ents) in scenario_ticks().iter().enumerate() {
        let sync = by_value.sync_stamps(api_stamps(ents));
        buffer.clear();
        buffer.extend(api_stamps(ents));
        let changed = by_buffer.sync_stamps_from(&mut buffer);
        assert_eq!(changed, sync.changed, "tick {tick}");
        assert_eq!(
            by_buffer.dirty_clusters(),
            sync.dirty_clusters.as_slice(),
            "tick {tick}"
        );
        assert_eq!(by_buffer.state_hash(), by_value.state_hash(), "tick {tick}");
        assert_eq!(
            by_buffer.stamps(),
            &buffer[..],
            "the buffer keeps the new set, sorted"
        );
        assert_eq!(by_buffer.generation(), by_value.generation(), "tick {tick}");
    }
}

/// The det-scan's float-ordering rule (ADR-8085 F1) on this file's two order calls: the grid's
/// `clamp_by_compare` is std's `f32::clamp(v, 0.0, hi)` bit for bit, edge values included, so
/// swapping it in moved no sim value (no sim-version change). The two test fixtures that keep a
/// std `clamp`/`min` never meet a tie of zeros or a NaN.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the std clamp is the reference this test compares the compare-based clamp with (ADR-8877)"
)]
fn clamp_by_compare_matches_std_clamp() {
    let edges = [
        0.0f32,
        -0.0,
        1.0e-30,
        -1.0e-30,
        0.5,
        94.999,
        95.0,
        95.0001,
        1.0e30,
        -1.0e30,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        -f32::NAN,
        f32::MIN_POSITIVE,
    ];
    for hi in [0.0f32, 1.0, 95.0, 255.0] {
        for v in edges {
            // det-scan: allow(std_float_fn): the std call is the reference this test compares with
            let want = v.clamp(0.0, hi);
            let got = clamp_by_compare(v, hi);
            assert_eq!(got.to_bits(), want.to_bits(), "clamp({v}, 0, {hi})");
        }
    }
    let mut rng = Lcg(0x5eed_c1a3);
    for _ in 0..10_000 {
        let v = rng.range(-200.0, 200.0);
        // det-scan: allow(std_float_fn): the std call is the reference this test compares with
        let want = v.clamp(0.0, 95.0);
        assert_eq!(clamp_by_compare(v, 95.0).to_bits(), want.to_bits());
    }
    // The fixture `funnel_patch` keeps a std `min(24.0)` on a non-negative drop (never two zeros,
    // never a NaN): recomputed by a comparison it has the same bits.
    let patch = funnel_patch(0.9);
    for (k, h) in patch.heights.iter().enumerate() {
        let c = k % patch.cols;
        let drop = c as f32 * 0.5 * 0.9;
        let want = -(if drop < 24.0 { drop } else { 24.0 });
        assert_eq!(h.to_bits(), want.to_bits());
    }
}
