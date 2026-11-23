// SPDX-License-Identifier: MPL-2.0
//! Forest flythrough (ADR-1503): corridor clearance and carve, placement queries, motion LOD,
//! wake sway, canopy gaps and the scripted flythrough, on a synthetic dense forest.
//!
//! Every test is deterministic (fixed paths, fixed steps, background streaming off where
//! counters are compared); none needs a GPU or a golden image.

use regia_terrain::scatter::forest::{
    CameraPath, CanopyGaps, CorridorSpec, FlightPlan, FlythroughSpec, ForestSpec, MotionLod, ScatterQuery,
    SyntheticForest, carve_corridor, corridor_report, flight_check, run_flythrough, synthetic_forest,
};
use regia_terrain::{ScatterField, ScatterSet, place_set};

fn small_forest(gaps: Option<CanopyGaps>) -> SyntheticForest {
    synthetic_forest(&ForestSpec {
        world_width_m: 1024.0,
        samples: 129,
        canopy_fade_m: [150.0, 200.0],
        understory_fade_m: [60.0, 80.0],
        gaps,
        gap_map_side: 513,
        ..ForestSpec::default()
    })
    .expect("forest")
}

/// A straight path `height` metres above the ground from `x0` to `x1` along `z`, a point every
/// `step` metres.
fn ground_path(forest: &SyntheticForest, x0: f32, x1: f32, z: f32, height: f32, step: f32) -> CameraPath {
    let n = ((x1 - x0).abs() / step).ceil() as usize;
    let points = (0..=n)
        .map(|i| {
            let x = x0 + (x1 - x0) * i as f32 / n as f32;
            [x, forest.source.map.sample_bilinear(x, z) + height, z]
        })
        .collect();
    CameraPath { points, times: None }
}

fn query(forest: &SyntheticForest, set: &ScatterSet) -> ScatterQuery {
    ScatterQuery::new(set, forest.source.clone(), &forest.library).expect("query")
}

fn field(forest: &SyntheticForest) -> ScatterField {
    let mut field =
        ScatterField::new(&forest.set, forest.source.clone(), &forest.library, 0x1503).expect("field");
    // Counters are compared between runs: place every needed tile on the calling thread.
    field.set_background_streaming(false);
    field.set_parallel_collect(false);
    field
}

#[test]
fn the_query_matches_placement() {
    let forest = small_forest(None);
    let q = query(&forest, &forest.set);
    let all = q.in_box([-600.0, -1.0e4, -600.0], [600.0, 1.0e4, 600.0], None);
    let placed = place_set(
        &forest.set,
        regia_terrain::PlacementInputs {
            map: &forest.source.map,
            holes: None,
            layers: None,
            density_maps: Some(&forest.source.density_maps),
        },
    )
    .expect("place");
    assert_eq!(all.len(), placed.len());
    let key = |c: u16, p: [f32; 3]| (c, p[0].to_bits(), p[1].to_bits(), p[2].to_bits());
    let mut a: Vec<_> = all.iter().map(|h| key(h.class, h.position)).collect();
    let mut b: Vec<_> = placed.iter().map(|p| key(p.class, p.position)).collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
    // Ids are unique and stable.
    let mut ids: Vec<u64> = all.iter().map(|h| h.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), all.len());
    assert_eq!(
        q.in_box([-600.0, -1.0e4, -600.0], [600.0, 1.0e4, 600.0], None),
        all
    );
    // A box above the canopy holds nothing; trees are 25-33 m tall at most.
    assert!(
        q.in_box([-50.0, 200.0, -50.0], [50.0, 300.0, 50.0], None)
            .is_empty()
    );
}

#[test]
fn a_canopy_path_reports_violations_and_the_carve_clears_them() {
    let forest = small_forest(None);
    let q = query(&forest, &forest.set);
    let path = ground_path(&forest, -200.0, 200.0, 7.0, 12.0, 25.0);
    let spec = CorridorSpec {
        clearance_m: 2.5,
        ..CorridorSpec::default()
    };
    let before = corridor_report(&q, &path, &spec);
    assert_eq!(before.segments.len(), path.points.len() - 1);
    assert!(before.obstacle_violations > 10, "{before:?}");
    assert!(before.violating_segments > 0);
    assert_eq!(before.terrain_violations, 0);
    assert!(
        before.min_clearance_m.is_some_and(|c| c < 0.0),
        "the path runs through crowns"
    );
    assert!(before.canopy_cover > 0.0 && before.canopy_cover <= 1.0);
    let carve = carve_corridor(&q, &path, &spec);
    assert_eq!(carve.before, before);
    let violators: u32 = carve.classes.iter().map(|c| c.violators).sum();
    assert_eq!(violators, before.obstacle_violations);
    assert!(carve.removed() >= violators);
    let mut carved = forest.set.clone();
    assert!(carve.apply(&mut carved) > 0);
    let q2 = query(&forest, &carved);
    let after = corridor_report(&q2, &path, &spec);
    assert_eq!(after.obstacle_violations, 0, "{after:?}");
    assert_eq!(after.violating_segments, 0);
    assert!(after.min_clearance_m.is_none_or(|c| c >= spec.clearance_m));
    // Nothing away from the corridor moved or vanished.
    let far = ([-300.0, -1.0e4, 150.0], [300.0, 1.0e4, 300.0]);
    assert_eq!(q.in_box(far.0, far.1, None), q2.in_box(far.0, far.1, None));
    // The carved set is a valid .scatter.ron v2 file and loads back the same.
    let text = carved.to_ron().expect("ron");
    let back = ScatterSet::from_ron(&text, None).expect("parse");
    assert_eq!(back, carved);
}

#[test]
fn a_path_above_the_canopy_needs_no_carve_and_low_paths_get_a_lift() {
    let forest = small_forest(None);
    let q = query(&forest, &forest.set);
    let high = ground_path(&forest, -150.0, 150.0, -40.0, 60.0, 30.0);
    let report = corridor_report(&q, &high, &CorridorSpec::default());
    assert_eq!(report.violating_segments, 0, "{report:?}");
    assert!(
        carve_corridor(&q, &high, &CorridorSpec::default())
            .classes
            .is_empty()
    );
    let low = ground_path(&forest, -150.0, 150.0, -40.0, 0.5, 30.0);
    let report = corridor_report(&q, &low, &CorridorSpec::default());
    assert!(report.terrain_violations > 0);
    assert!(report.segments.iter().any(|s| s.suggested_lift_m >= 1.0));
}

#[test]
fn gaps_open_the_canopy_overhead() {
    let closed = small_forest(None);
    let open = small_forest(Some(CanopyGaps {
        gap_fraction: 0.3,
        // Clearings wider than a crown, so the sky opens over them.
        min_area_m2: 400.0,
        max_area_m2: 6000.0,
        zone_min: [-400.0, -400.0],
        zone_max: [400.0, 400.0],
        ..CanopyGaps::default()
    }));
    let path = ground_path(&closed, -300.0, 300.0, 20.0, 4.0, 20.0);
    let spec = CorridorSpec {
        classes: vec!["canopy_broadleaf".into(), "canopy_conifer".into()],
        ..CorridorSpec::default()
    };
    let a = corridor_report(&query(&closed, &closed.set), &path, &spec);
    let b = corridor_report(&query(&open, &open.set), &path, &spec);
    // Gaps only remove trees, so cover can only fall; at 30 % gaps it falls visibly.
    assert!(
        b.canopy_cover < a.canopy_cover,
        "closed {} open {}",
        a.canopy_cover,
        b.canopy_cover
    );
    assert!(b.segments.iter().any(|s| s.canopy_cover < 0.5));
}

fn canopy_flight(forest: &SyntheticForest, height: f32) -> CameraPath {
    ground_path(forest, -150.0, 150.0, 3.0, height, 10.0)
}

#[test]
fn the_flythrough_is_deterministic() {
    let forest = small_forest(None);
    let path = canopy_flight(&forest, 6.0);
    let spec = FlythroughSpec {
        max_frames: 90,
        ..FlythroughSpec::default()
    };
    let a = run_flythrough(&mut field(&forest), &path, &spec);
    let b = run_flythrough(&mut field(&forest), &path, &spec);
    assert_eq!(a, b);
    assert_eq!(a.frames.len(), 90);
    assert!(a.max_emitted > 500, "a dense forest: {}", a.max_emitted);
    assert!(a.cpu_us.is_none());
}

#[test]
fn motion_lod_keeps_lod_switches_out_of_the_near_field() {
    let forest = small_forest(None);
    let path = canopy_flight(&forest, 3.0);
    let spec = FlythroughSpec {
        max_frames: 150,
        record_switches: true,
        ..FlythroughSpec::default()
    };
    let motion = MotionLod::for_flight(spec.speed_m_s, spec.fps);
    let near = 0.6 * motion.full_detail_m;
    let inner = |r: &regia_terrain::scatter::forest::FlythroughReport| {
        r.switches.iter().filter(|s| s.distance_m < near).count()
    };
    let plain = run_flythrough(&mut field(&forest), &path, &spec);
    let mut tuned_field = field(&forest);
    tuned_field.set_motion_lod(Some(motion));
    let tuned = run_flythrough(&mut tuned_field, &path, &spec);
    assert!(inner(&plain) > 0, "the plain field switches LODs near the lens");
    assert_eq!(
        inner(&tuned),
        0,
        "{:?}",
        tuned
            .switches
            .iter()
            .filter(|s| s.distance_m < near)
            .take(5)
            .collect::<Vec<_>>()
    );
    // Cross-fades keep every switch soft either way.
    assert_eq!(plain.pops.hard_switches, 0);
    assert_eq!(tuned.pops.hard_switches, 0);
    // Off again: the frame is the plain one.
    let mut off = field(&forest);
    off.set_motion_lod(Some(motion));
    off.set_motion_lod(None);
    assert_eq!(run_flythrough(&mut off, &path, &spec), plain);
}

#[test]
fn the_wake_sways_trees_and_still_air_changes_nothing() {
    let forest = small_forest(None);
    let path = canopy_flight(&forest, 8.0);
    let base = FlythroughSpec {
        max_frames: 120,
        ..FlythroughSpec::default()
    };
    let plain = run_flythrough(&mut field(&forest), &path, &base);
    let windy = run_flythrough(&mut field(&forest), &path, &FlythroughSpec { sway: true, ..base });
    assert!(windy.max_tilt_mrad > 0);
    assert!(windy.frames.iter().any(|f| f.swaying > 0));
    // Leaning never adds or removes draws.
    let draws = |r: &regia_terrain::scatter::forest::FlythroughReport| {
        r.frames.iter().map(|f| f.draws).collect::<Vec<_>>()
    };
    assert_eq!(draws(&plain), draws(&windy));
    // A wake that never starts (threshold above the flight speed) is the plain frame.
    let still = run_flythrough(
        &mut field(&forest),
        &path,
        &FlythroughSpec {
            sway: true,
            wake: regia_terrain::scatter::forest::WakeModel {
                min_speed_m_s: 1000.0,
                ..Default::default()
            },
            ..base
        },
    );
    assert_eq!(still.max_tilt_mrad, 0);
    assert_eq!(still.pops, plain.pops);
    assert_eq!(draws(&still), draws(&plain));
}

#[test]
fn the_flight_check_flags_the_standin_near_lod_switches() {
    let forest = small_forest(None);
    let slow = flight_check(&forest.set, Some(&forest.library), &FlightPlan::default());
    let canopy = &slow.classes[0];
    assert_eq!(canopy.class, "canopy_broadleaf");
    assert!(
        canopy.lod0_end_m.is_some_and(|d| d > 10.0 && d < 100.0),
        "{canopy:?}"
    );
    assert!(canopy.band_crossing_s >= 0.25);
    // The stand-ins leave LOD 0 close by design: the check says so at 100 m/s.
    assert!(
        slow.classes
            .iter()
            .any(|c| c.warnings.contains(&"lod0_switch_near"))
    );
    assert!(slow.motion.full_detail_m > 30.0);
}

#[test]
fn the_flight_system_installs_a_carve_applies_motion_lod_and_sways_scatter_draws() {
    use regia_ecs::ecs::system::RunSystemOnce;
    use regia_ecs::prelude::World;
    use regia_render::{ExtractedCamera, ExtractedFrame, ExtractedTarget, ExtractedView, PostSettings};
    use regia_terrain::scatter::forest::{ForestFlight, SwaySettings, forest_flight_system};
    use regia_terrain::{RuntimeScatter, extract_runtime_scatter};

    let forest = small_forest(None);
    let path = ground_path(&forest, -150.0, 150.0, 7.0, 12.0, 25.0);
    let spec = CorridorSpec {
        clearance_m: 2.5,
        ..CorridorSpec::default()
    };
    let mut flight = ForestFlight::with_query(
        query(&forest, &forest.set),
        Some(std::sync::Arc::new(forest.library.clone())),
        0x1503,
    );
    let carve = carve_corridor(flight.query().expect("query"), &path, &spec);
    let mut carved = forest.set.clone();
    assert!(carve.apply(&mut carved) > 0);
    flight.install_set(carved).expect("install");
    flight.motion = Some(MotionLod::default());
    flight.sway = Some(SwaySettings::default());
    let mut world = World::new();
    world.insert_resource(RuntimeScatter::new(field(&forest)));
    world.insert_resource(flight);
    let mut swayed = 0u32;
    let mut leaned = 0u32;
    for k in 0..30 {
        // 100 m/s along the carved path.
        let x = -150.0 + 100.0 * k as f32 / 60.0;
        let eye = [x, forest.source.map.sample_bilinear(x, 7.0) + 12.0, 7.0];
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye,
                target: [eye[0] + 1.0, eye[1], eye[2]],
                fov_y: 0.9,
                near: 0.1,
                far: 2000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
        world.insert_resource(frame);
        world.run_system_once(extract_runtime_scatter).expect("extract");
        world.run_system_once(forest_flight_system).expect("flight");
        let stats = &world.resource::<ForestFlight>().stats;
        swayed = swayed.max(stats.sway.swaying);
        leaned = leaned.max(stats.leaned);
    }
    let flight = world.resource::<ForestFlight>();
    assert_eq!(flight.stats.rebuilds, 1, "{:?}", flight.stats.last_error);
    assert!(!flight.has_pending_set());
    assert_eq!(flight.stats.motion, Some(MotionLod::default()));
    assert!(swayed > 0 && leaned > 0, "swayed {swayed} leaned {leaned}");
    // The running field is the carved one: the path is clear through it.
    let q2 = flight.query().expect("query");
    assert_eq!(corridor_report(q2, &path, &spec).obstacle_violations, 0);
    assert_eq!(
        world.resource_mut::<RuntimeScatter>().field_mut().motion_lod(),
        Some(MotionLod::default())
    );
}
