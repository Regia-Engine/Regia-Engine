// SPDX-License-Identifier: MPL-2.0
//! Spec 52 gap tests (`docs/specs/52-motion-matching.md` § 8): channels, normalization modes,
//! exactness of every search method, `Auto`, cadence (urgent search, stagger, play rate, the
//! per-frame budget), the candidate report, the player and the baseline regression traces.
//! Deterministic: fixed clips, seeded PCG32 data, fixed steps; no wall clock.

use super::*;

/// Clips of the synthetic circular walk: straight, turning toward +X (`R = 2`) and toward −X.
fn circle_clips() -> Vec<AnimationClip> {
    vec![
        locomotion("straight", 1.5, 0.0, 0.3, 1.0, 2.0),
        locomotion("turn_pos", 1.5, 0.75, 0.3, 1.0, 2.0),
        locomotion("turn_neg", 1.5, -0.75, 0.3, 1.0, 2.0),
    ]
}

fn local_index(db: &MotionDatabase, frame: usize) -> (usize, usize) {
    let at = db.frame(frame).expect("frame");
    (at.clip, frame - db.clips()[at.clip].start)
}

fn all_methods(db: &MotionDatabase, q: &[f32]) -> SearchHit {
    let brute = db
        .search(q, Exclusion::NONE, SearchMethod::BruteForce)
        .expect("hit");
    for method in [SearchMethod::KdTree, SearchMethod::Auto] {
        let hit = db.search(q, Exclusion::NONE, method).expect("hit");
        assert_eq!(hit.frame, brute.frame, "{method:?}");
        assert_eq!(hit.cost.to_bits(), brute.cost.to_bits(), "{method:?}");
    }
    brute
}

#[test]
fn circular_walk_picks_the_turn_the_path_asks_for() {
    let clips = circle_clips();
    let db = build(&clips);
    let turn_start = db.clips()[1].start;
    let frame = turn_start + 12;
    let own = db.frame_query(frame).expect("query");
    // Its own path: the frame itself (cost 0).
    let hit = all_methods(&db, &db.normalize(&own).expect("normalize"));
    assert_eq!(hit.frame, frame);
    // The same pose with each clip's path at the same phase picks that clip at that phase.
    for clip in 0..3 {
        let path = db
            .frame_query(db.clips()[clip].start + 12)
            .expect("query")
            .trajectory;
        let q = db.query_from_frame(frame, &path).expect("query");
        let hit = all_methods(&db, &db.normalize(&q).expect("normalize"));
        // The clips are periodic in the character frame (one stride per second, 30 frames), so
        // the same phase one stride later is an equally good answer.
        let (got_clip, local) = local_index(&db, hit.frame);
        assert_eq!((got_clip, local % 30), (clip, 12), "path of clip {clip}");
    }
}

#[test]
fn weights_choose_what_the_answer_depends_on() {
    let clips = circle_clips();
    // Only the path counts: a mean pose with the turn_neg path finds turn_neg.
    let mut path_only = schema();
    path_only.weights = FeatureWeights {
        root_velocity: 0.0,
        trajectory_position: 1.0,
        trajectory_direction: 0.0,
        foot_position: 0.0,
        foot_velocity: 0.0,
        ..FeatureWeights::default()
    };
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), path_only).expect("db");
    let path = db.frame_query(db.clips()[2].start + 5).expect("query").trajectory;
    let hit = all_methods(&db, &db.normalize(&db.query_with_mean_pose(&path)).expect("n"));
    assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(2));

    // Only the feet count: the answer follows the foot phase and ignores the path (every clip
    // has the same feet, so the tie goes to the lowest index: the straight clip).
    let mut feet_only = schema();
    feet_only.weights = FeatureWeights {
        root_velocity: 0.0,
        trajectory_position: 0.0,
        trajectory_direction: 0.0,
        foot_position: 1.0,
        foot_velocity: 0.0,
        ..FeatureWeights::default()
    };
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), feet_only).expect("db");
    let frame = db.clips()[2].start + 17;
    let q = db.query_from_frame(frame, &straight(4.0)).expect("query");
    let hit = all_methods(&db, &db.normalize(&q).expect("n"));
    assert_eq!(local_index(&db, hit.frame).1 % 30, 17, "same foot phase");
}

#[test]
fn speed_picks_the_matching_walk_and_mid_speed_matches_the_hand_computed_cost() {
    let db = build(&[walk(), locomotion("walk_fast", 2.0, 0.0, 0.3, 1.0, 1.0)]);
    let pick = |speed: f32| {
        let q = db
            .query_from_frame(db.clips()[0].start + 3, &straight(speed))
            .expect("q");
        let n = db.normalize(&q).expect("n");
        (all_methods(&db, &n), n)
    };
    assert_eq!(db.frame(pick(2.0).0.frame).map(|f| f.clip), Some(1));
    assert_eq!(db.frame(pick(1.5).0.frame).map(|f| f.clip), Some(0));
    let (hit, n) = pick(1.75);
    // The hand-computed cost of the winner: the sum of squared differences in f64.
    let features = db.features(hit.frame).expect("features");
    let manual: f64 = n
        .iter()
        .zip(features)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum();
    assert!(
        (f64::from(hit.cost) - manual).abs() <= 1e-5 * manual.max(1.0),
        "{} vs {manual}",
        hit.cost
    );
}

fn history_schema() -> FeatureSchema {
    let mut schema = schema();
    schema.history_times = vec![0.25, 0.5];
    schema
}

#[test]
fn history_layout_appends_and_leaves_the_baseline_floats_untouched() {
    let clips = circle_clips();
    let base = build(&clips);
    let with = MotionDatabase::build(&skeleton(), &looping(&clips), history_schema()).expect("db");
    assert_eq!(base.dimensions(), 27);
    assert_eq!(with.dimensions(), 31);
    for group in FeatureGroup::BASELINE {
        assert_eq!(base.schema().group_range(group), with.schema().group_range(group));
    }
    assert_eq!(with.schema().group_range(FeatureGroup::TrajectoryHistory), 27..31);
    for frame in 0..base.len() {
        let a = base.raw_features(frame).expect("raw");
        let b = with.raw_features(frame).expect("raw");
        assert!(
            a.iter().zip(&b[..27]).all(|(x, y)| x.to_bits() == y.to_bits()),
            "frame {frame}: the baseline floats changed"
        );
    }
    // A schema with no history keeps the baseline layout bit for bit.
    assert_eq!(base.schema().group_range(FeatureGroup::TrajectoryHistory), 27..27);
}

#[test]
fn history_wraps_a_looping_clip_and_clamps_one_that_does_not() {
    let clip = locomotion("straight", 1.5, 0.0, 0.3, 1.0, 2.0);
    let looped = MotionDatabase::build(
        &skeleton(),
        &looping(std::slice::from_ref(&clip)),
        history_schema(),
    )
    .expect("db");
    let q = looped.frame_query(0).expect("query");
    close2(
        q.history[0],
        Vec2::new(0.0, -0.375),
        1e-4,
        "0.25 s back, previous lap",
    );
    close2(
        q.history[1],
        Vec2::new(0.0, -0.75),
        1e-4,
        "0.5 s back, previous lap",
    );
    let once = MotionDatabase::build(
        &skeleton(),
        &[MotionClip {
            clip: &clip,
            looping: false,
        }],
        history_schema(),
    )
    .expect("db");
    let q = once.frame_query(0).expect("query");
    close2(q.history[0], Vec2::ZERO, 1e-5, "clamped to the start");
    let q = once
        .frame_query(once.frame_at(0, 1.0).expect("frame"))
        .expect("query");
    close2(q.history[1], Vec2::new(0.0, -0.75), 1e-4, "inside the clip");
}

#[test]
fn history_separates_clips_that_differ_only_in_their_past() {
    let clips = circle_clips();
    let mut schema = history_schema();
    schema.weights = FeatureWeights {
        root_velocity: 0.0,
        trajectory_position: 0.0,
        trajectory_direction: 0.0,
        foot_position: 0.0,
        foot_velocity: 0.0,
        trajectory_history: 1.0,
        ..FeatureWeights::default()
    };
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), schema).expect("db");
    let from_pos = db.frame_query(db.clips()[1].start + 20).expect("query");
    let hit = all_methods(&db, &db.normalize(&from_pos).expect("n"));
    assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(1));
    let mut mirrored = from_pos.clone();
    for p in &mut mirrored.history {
        p.x = -p.x;
    }
    let hit = all_methods(&db, &db.normalize(&mirrored).expect("n"));
    assert_eq!(
        db.frame(hit.frame).map(|f| f.clip),
        Some(2),
        "the mirrored past is the other turn"
    );
}

#[test]
fn the_runtime_history_comes_from_the_recorded_ground_track() {
    let clips = circle_clips();
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), history_schema()).expect("db");
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    // Walk straight along +Z at 1.5 m/s, recording the ground every update.
    for i in 0..30 {
        matcher.record_ground(Vec3::new(0.0, 0.0, 1.5 * i as f32 * DT), 0.0, &db);
        matcher.update(&db, &straight(1.5), DT).expect("update");
    }
    matcher.record_ground(Vec3::new(0.0, 0.0, 1.5 * 30.0 * DT), 0.0, &db);
    let q = matcher.current_query(&db, &straight(1.5)).expect("query");
    close2(q.history[0], Vec2::new(0.0, -0.375), 1e-3, "recorded 0.25 s back");
    close2(q.history[1], Vec2::new(0.0, -0.75), 1e-3, "recorded 0.5 s back");
    // Before the buffer covers the time, it extrapolates backward at the current velocity.
    let mut fresh = MotionMatcher::new(MatcherSettings::default());
    fresh.record_ground(Vec3::ZERO, 0.0, &db);
    fresh.update(&db, &straight(1.5), DT).expect("update");
    fresh.record_ground(Vec3::new(0.0, 0.0, 0.05), 0.0, &db);
    let q = fresh.current_query(&db, &straight(1.5)).expect("query");
    close2(q.history[0], Vec2::new(0.0, -0.375), 1e-3, "extrapolated");
}

/// A root, two feet and a hand.
fn skeleton_with_hand() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "foot_l".into(), "foot_r".into(), "hand".into()],
        vec![None, Some(ROOT), Some(ROOT), Some(ROOT)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(-0.1, 0.0, 0.0)),
            translated(Vec3::new(0.1, 0.0, 0.0)),
            translated(Vec3::new(0.3, 1.0, 0.0)),
        ],
    )
    .expect("valid skeleton")
}

#[test]
fn pose_joints_add_position_and_velocity_groups() {
    let mut schema = schema();
    schema.pose_joints = vec![PoseJoint {
        joint: 3,
        position: true,
        velocity: false,
        weight: 2.0,
    }];
    let clips = [walk()];
    let db = MotionDatabase::build(&skeleton_with_hand(), &looping(&clips), schema.clone()).expect("db");
    assert_eq!(db.dimensions(), 30);
    assert_eq!(schema.group_range(FeatureGroup::PosePosition), 27..30);
    assert!(schema.group_range(FeatureGroup::PoseVelocity).is_empty());
    let q = db.frame_query(4).expect("query");
    close3(
        q.pose_joints[0].position,
        Vec3::new(0.3, 1.0, 0.0),
        1e-4,
        "hand relative to the ground",
    );
    assert_eq!(q.pose_joints[0].velocity, Vec3::ZERO, "no velocity feature");
    // The joint weight multiplies its dimensions: with `none` normalization the scale is it.
    schema.normalization = NormalizationMode::None;
    let db = MotionDatabase::build(&skeleton_with_hand(), &looping(&clips), schema).expect("db");
    assert!(db.scale()[27..30].iter().all(|&s| s == 2.0));
    assert!(db.scale()[..27].iter().all(|&s| s == 1.0));
}

#[test]
fn schema_validation_names_each_new_error() {
    let skeleton = skeleton_with_hand();
    let clips = [walk()];
    let build_with = |edit: &dyn Fn(&mut FeatureSchema)| {
        let mut schema = schema();
        edit(&mut schema);
        MotionDatabase::build(&skeleton, &looping(&clips), schema).err()
    };
    assert_eq!(
        build_with(&|s| s.history_times = vec![0.2, -1.0]),
        Some(MotionMatchError::BadHistoryTime(-1.0))
    );
    assert_eq!(
        build_with(&|s| s.pose_joints = vec![PoseJoint::new(FOOT_L)]),
        Some(MotionMatchError::DuplicateJoint(FOOT_L))
    );
    assert!(matches!(
        build_with(&|s| s.trajectory_times = (1..=17).map(|i| i as f32 * 0.1).collect()),
        Some(MotionMatchError::TooManyDimensions {
            what: "trajectory samples",
            got: 17,
            max: 16
        })
    ));
    assert!(matches!(
        build_with(&|s| s.history_times = (1..=9).map(|i| i as f32 * 0.1).collect()),
        Some(MotionMatchError::TooManyDimensions {
            what: "history samples",
            got: 9,
            ..
        })
    ));
    assert!(matches!(
        build_with(&|s| s.sample_weights.trajectory = vec![1.0, 2.0]),
        Some(MotionMatchError::SampleWeightCount {
            got: 2,
            expected: 3,
            ..
        })
    ));
    assert!(matches!(
        build_with(&|s| s.sample_weights.trajectory = vec![1.0, 101.0, 1.0]),
        Some(MotionMatchError::BadWeight { value, .. }) if value == 101.0
    ));
    assert!(matches!(
        build_with(&|s| s.weights.foot_position = 1001.0),
        Some(MotionMatchError::BadWeight {
            group: "foot_position",
            ..
        })
    ));
    assert!(matches!(
        build_with(&|s| s.pose_joints = vec![PoseJoint {
            weight: f32::NAN,
            ..PoseJoint::new(3)
        }]),
        Some(MotionMatchError::BadWeight { .. })
    ));
    assert!(matches!(
        build_with(&|s| s.pose_joints = vec![PoseJoint::new(9)]),
        Some(MotionMatchError::JointOutOfRange { joint: 9, .. })
    ));
    // Trajectory-only matching (no feet, no pose joints) is valid.
    assert_eq!(build_with(&|s| s.feet.clear()), None);
}

#[test]
fn normalization_modes_and_sample_weights_set_the_scale() {
    let clips = circle_clips();
    let mut none = schema();
    none.normalization = NormalizationMode::None;
    none.weights.foot_position = 3.0;
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), none.clone()).expect("db");
    let feet = none.group_range(FeatureGroup::FootPosition);
    assert!(db.scale()[feet].iter().all(|&s| s == 3.0));

    // Per-dimension: every non-constant dimension normalizes to its weight's RMS.
    let mut per = schema();
    per.normalization = NormalizationMode::PerDimension;
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), per).expect("db");
    let n = db.len() as f64;
    for dim in 0..db.dimensions() {
        let rms = ((0..db.len())
            .map(|f| f64::from(db.features(f).expect("f")[dim]).powi(2))
            .sum::<f64>()
            / n)
            .sqrt();
        assert!(rms < 1e-4 || (rms - 1.0).abs() < 1e-3, "dim {dim} rms {rms}");
    }

    // Sample weights scale one sample's dimensions inside the group.
    let mut weighted = schema();
    weighted.sample_weights.trajectory = vec![4.0, 1.0, 1.0];
    let plain = build(&clips);
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), weighted.clone()).expect("db");
    for group in [
        FeatureGroup::TrajectoryPosition,
        FeatureGroup::TrajectoryDirection,
    ] {
        let r = weighted.group_range(group);
        close(
            db.scale()[r.start],
            4.0 * plain.scale()[r.start],
            1e-6,
            "first sample x4",
        );
        close(
            db.scale()[r.start + 2],
            plain.scale()[r.start + 2],
            1e-6,
            "second sample x1",
        );
    }
}

// ---- exactness (§ 2.7, § 8 item 7) --------------------------------------------------------------

#[test]
fn every_method_and_top_k_agree_on_random_databases() {
    let sizes = [
        1, 2, 3, 7, 8, 9, 31, 64, 257, 500, 1000, 1500, 2000, 3000, 5000, 6000, 8000, 10_000, 15_000, 20_000,
    ];
    let dims_list = [8, 9, 15, 16, 27, 31, 48, 64, 95, 96];
    let mut rng = Rng::new(0x0052_eac7);
    for (index, &rows) in sizes.iter().enumerate() {
        let dims = dims_list[index % dims_list.len()];
        let quantize = index % 2 == 0;
        let data = random_rows(&mut rng, rows, dims, quantize);
        let biases: Vec<f32> = (0..rows)
            .map(|_| if rng.chance(0.3) { rng.f32() - 0.5 } else { 0.0 })
            .collect();
        let min_bias = biases.iter().copied().fold(0.0f32, f32::min);
        let tree = KdTree::build_with_leaf_size(&data, dims, 4 + index % 29).expect("tree");
        let queries = if rows <= 5000 { 1000 } else { 200 };
        for q in 0..queries {
            let query: Vec<f32> = if q % 3 == 0 {
                // Exactly a stored row: forces ties with its duplicates.
                let row = (rng.next_u32() as usize) % rows;
                data[row * dims..(row + 1) * dims].to_vec()
            } else {
                random_rows(&mut rng, 1, dims, quantize)
            };
            let skip = (rng.next_u32() as usize) % rows.max(1);
            let excluded = |row: usize| row % 97 == skip % 97 && rows > 1;
            let bias = |row: usize| biases[row];
            let (brute, _) = nearest_brute_force_biased_counted(&data, dims, &query, excluded, bias);
            let (tree_hit, _) = tree.nearest_biased_counted(&query, excluded, bias, min_bias);
            assert_eq!(
                brute.map(|n| n.row),
                tree_hit.map(|n| n.row),
                "rows {rows} dims {dims} q {q}"
            );
            assert_eq!(
                brute.map(|n| n.distance.to_bits()),
                tree_hit.map(|n| n.distance.to_bits()),
                "rows {rows} dims {dims} q {q}"
            );
            if q % 10 == 0 {
                let k = 1 + (rng.next_u32() as usize) % 32;
                let (top, _) = tree.nearest_k_biased(&query, excluded, bias, min_bias, k);
                let mut all: Vec<(f32, usize)> = (0..rows)
                    .filter(|&r| !excluded(r))
                    .map(|r| {
                        (
                            squared_distance(&query, &data[r * dims..(r + 1) * dims]) + biases[r],
                            r,
                        )
                    })
                    .collect();
                all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                all.truncate(k);
                let got: Vec<(u32, usize)> = top.iter().map(|n| (n.distance.to_bits(), n.row)).collect();
                let want: Vec<(u32, usize)> = all.iter().map(|&(d, r)| (d.to_bits(), r)).collect();
                assert_eq!(got, want, "top-{k}: rows {rows} dims {dims}");
            }
        }
    }
}

/// Two long looping clips: 2,400 frames, past the `Auto` tree threshold.
fn big_tagged_db() -> MotionDatabase {
    let mut db = build(&[
        locomotion("walk_long", 1.5, 0.0, 0.3, 1.0, 40.0),
        locomotion("run_long", 4.0, 0.0, 0.6, 0.5, 40.0),
    ]);
    let rare = SpanKind::Tag(0b10);
    db.annotate(
        tag_table(),
        &[
            ClipAnnotation {
                tags: 0b01,
                spans: Vec::new(),
            },
            ClipAnnotation {
                tags: 0,
                spans: vec![Span {
                    start: 0.0,
                    end: 4.0,
                    kind: rare,
                }],
            },
        ],
    )
    .expect("annotate");
    db
}

#[test]
fn auto_picks_the_tree_or_the_scan_and_never_changes_the_answer() {
    let small = build(&[walk(), run()]);
    assert_eq!(
        small.resolve_method(SearchMethod::Auto, SearchFilter::NONE),
        SearchMethod::BruteForce,
        "under 2,048 frames"
    );
    let db = big_tagged_db();
    assert_eq!(db.len(), 2400);
    assert_eq!(db.tag_count(0), 1200);
    assert_eq!(db.tag_count(1), 120);
    assert_eq!(
        db.resolve_method(SearchMethod::Auto, SearchFilter::NONE),
        SearchMethod::KdTree
    );
    let rare = SearchFilter {
        require: 0b10,
        forbid: 0,
    };
    assert_eq!(
        db.resolve_method(SearchMethod::Auto, rare),
        SearchMethod::BruteForce,
        "120 of 2,400 frames eligible"
    );
    assert_eq!(
        db.resolve_method(SearchMethod::KdTree, rare),
        SearchMethod::KdTree
    );
    for speed in [1.0, 1.5, 2.5, 4.0] {
        let q = db
            .normalize(&db.query_with_mean_pose(&straight(speed)))
            .expect("normalize");
        for filter in [SearchFilter::NONE, rare] {
            let auto = db.search_detailed(&q, Exclusion::NONE, filter, 0.0, |_| false, SearchMethod::Auto);
            for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
                let other = db.search_detailed(&q, Exclusion::NONE, filter, 0.0, |_| false, method);
                assert_eq!(auto.hit.map(|h| h.frame), other.hit.map(|h| h.frame));
                assert_eq!(
                    auto.hit.map(|h| h.cost.to_bits()),
                    other.hit.map(|h| h.cost.to_bits())
                );
                assert_eq!(other.method_used, method);
            }
            if filter == rare {
                assert_eq!(auto.method_used, SearchMethod::BruteForce);
                let frame = auto.hit.expect("hit").frame;
                assert!(db.frame_meta(frame).expect("meta").tags & 0b10 != 0);
            } else {
                assert_eq!(auto.method_used, SearchMethod::KdTree);
                assert!(auto.work.nodes_visited > 0);
            }
        }
    }
}

#[test]
fn unindexed_frames_leave_top_k_entirely() {
    let mut db = build(&[walk(), run()]);
    let q = walk_query(&db);
    let before = db.search_top_k(&q, SearchFilter::NONE, Exclusion::NONE, 32);
    assert_eq!(before.len(), 32);
    db.annotate(
        TagTable::default(),
        &[
            ClipAnnotation {
                tags: 0,
                spans: vec![Span {
                    start: 0.0,
                    end: 1.0,
                    kind: SpanKind::Unindexed,
                }],
            },
            ClipAnnotation::default(),
        ],
    )
    .expect("annotate");
    let after = db.search_top_k(&q, SearchFilter::NONE, Exclusion::NONE, 32);
    assert!(after.iter().all(|h| db.frame(h.frame).map(|f| f.clip) == Some(1)));
    assert_eq!(db.target_count(), db.clips()[1].frames);
}

// ---- cadence (§ 2.8, § 8 items 8 and 9) ---------------------------------------------------------

/// The desired path of the 200-step regression trace: walk, run, turn, walk.
fn trace_path(step: usize) -> Vec<TrajectoryPoint> {
    let phase = step / 50;
    let speed = if phase.is_multiple_of(2) { 1.4 } else { 3.8 };
    let turn = if phase == 2 { 0.9 } else { 0.0 };
    DEFAULT_TRAJECTORY_TIMES
        .iter()
        .map(|&t| {
            if turn == 0.0 {
                TrajectoryPoint::ahead(speed * t)
            } else {
                let r = speed / turn;
                let a = turn * t;
                TrajectoryPoint {
                    position: Vec2::new(r * (1.0 - a.cos()), r * a.sin()),
                    direction: Vec2::new(a.sin(), a.cos()),
                }
            }
        })
        .collect()
}

/// The jump sequence (`step:clip:frame,`) of 200 default-settings updates over `trace_path`.
fn jump_trace() -> String {
    let db = build(&[walk(), run(), locomotion("turn", 2.0, 1.0, 0.4, 1.0, 1.0)]);
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    let mut out = String::new();
    for step in 0..200 {
        let result = matcher.update(&db, &trace_path(step), DT).expect("update");
        if result.jumped {
            out.push_str(&format!(
                "{step}:{}:{},",
                result.playback.clip, result.playback.frame
            ));
        }
    }
    out
}

#[test]
fn default_jump_rule_equals_the_baseline_trace() {
    // Recorded from origin/main 28fb09309 (the baseline matcher) before spec 52's changes.
    assert_eq!(jump_trace(), "0:0:0,102:2:73,150:1:46,");
    let db = build(&[walk(), run(), locomotion("turn", 2.0, 1.0, 0.4, 1.0, 1.0)]);
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.1,
        transition_cost: 0.02,
        ..MatcherSettings::default()
    });
    let mut out = String::new();
    for step in 0..200 {
        let result = matcher.update(&db, &trace_path(step), DT).expect("update");
        if result.jumped {
            out.push_str(&format!(
                "{step}:{}:{},",
                result.playback.clip, result.playback.frame
            ));
        }
    }
    assert_eq!(out, "0:0:0,100:2:71,150:1:46,");
}

fn turning(sign: f32) -> Vec<TrajectoryPoint> {
    DEFAULT_TRAJECTORY_TIMES
        .iter()
        .map(|&t| {
            let (r, a) = (2.0 * sign, 0.75 * t);
            TrajectoryPoint {
                position: Vec2::new(r * (1.0 - a.cos()), 2.0 * a.sin()),
                direction: Vec2::new(sign * a.sin(), a.cos()),
            }
        })
        .collect()
}

#[test]
fn an_urgent_path_change_searches_before_the_interval() {
    let db = build(&circle_clips());
    let run = |urgent: Option<f32>| {
        let mut matcher = MotionMatcher::new(MatcherSettings {
            search_interval: 1.0,
            min_jump_interval: 0.0,
            urgent_threshold: urgent,
            ..MatcherSettings::default()
        });
        for _ in 0..5 {
            matcher.update(&db, &turning(1.0), DT).expect("update");
        }
        let step = matcher.update(&db, &turning(-1.0), DT).expect("update");
        (step, matcher)
    };
    let (calm, _) = run(None);
    assert!(!calm.searched, "the interval has not passed");
    let (urgent, matcher) = run(Some(0.5));
    assert!(urgent.searched, "the flipped path searched at once");
    assert!(matcher.last_search().is_some_and(|r| r.urgent));
    assert_eq!(
        db.frame(urgent.playback.frame).map(|f| f.clip),
        Some(2),
        "now on the other turn"
    );
    // A path that barely changes does not trigger it.
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 1.0,
        urgent_threshold: Some(0.5),
        ..MatcherSettings::default()
    });
    for i in 0..20 {
        let step = matcher
            .update(&db, &straight(1.5 + 0.001 * i as f32), DT)
            .expect("update");
        assert_eq!(step.searched, i == 0, "update {i}");
    }
}

#[test]
fn stagger_slots_offset_the_searches_and_repeat_exactly() {
    let db = build(&[walk(), run()]);
    let settings = MatcherSettings {
        search_interval: 0.4,
        ..MatcherSettings::default()
    };
    let searches = |slot: u32| {
        let mut matcher = MotionMatcher::new(settings);
        matcher.set_stagger_slot(slot);
        (0..60)
            .filter(|_| matcher.update(&db, &straight(1.5), DT).expect("update").searched)
            .collect::<Vec<usize>>()
    };
    let a = searches(1);
    let b = searches(5);
    assert_ne!(a, b, "different slots search on different updates");
    assert_eq!(a, searches(1), "the same slot repeats");
    assert_eq!((a[0], b[0]), (0, 0), "the first pick is immediate");
    assert!(
        b[1] < a[1],
        "slot 5 searches earlier after the first pick: {a:?} {b:?}"
    );
    // Slot 0 and stagger off both reproduce the baseline cadence.
    let mut slot0 = MotionMatcher::new(settings);
    let mut off = MotionMatcher::new(MatcherSettings {
        stagger: false,
        ..settings
    });
    off.set_stagger_slot(5);
    for _ in 0..60 {
        let x = slot0.update(&db, &straight(1.5), DT).expect("update");
        let y = off.update(&db, &straight(1.5), DT).expect("update");
        assert_eq!(x, y);
    }
}

#[test]
fn play_rate_scales_playback_time_only() {
    let db = build(&[walk(), run()]);
    let settings = MatcherSettings {
        transition_cost: 1000.0,
        ..MatcherSettings::default()
    };
    let mut normal = MotionMatcher::new(settings);
    let mut fast = MotionMatcher::new(MatcherSettings {
        play_rate: 2.0,
        ..settings
    });
    for _ in 0..10 {
        normal.update(&db, &straight(1.5), DT).expect("update");
        fast.update(&db, &straight(1.5), DT).expect("update");
    }
    let (n, f) = (normal.playback().expect("p"), fast.playback().expect("p"));
    close(f.time, 2.0 * n.time, 1e-5, "twice the clip time");
    assert_eq!(normal.searches(), fast.searches(), "searches keep real time");
    fast.settings.play_rate = f32::NAN;
    fast.update(&db, &straight(1.5), DT).expect("update");
    assert!(fast.playback().is_some_and(|p| p.time.is_finite()));
}

#[test]
fn an_empty_filter_after_the_first_pick_reports_no_candidates() {
    let db = tagged_db();
    let mut matcher = MotionMatcher::new(eager());
    matcher.update(&db, &straight(1.5), DT).expect("update");
    let first = matcher.last_search().expect("report");
    assert!(!first.filter_ignored);
    matcher.filter = SearchFilter {
        require: 0b01,
        forbid: 0b01,
    };
    let playing = matcher.playback();
    matcher.update(&db, &straight(4.0), DT).expect("update");
    let report = matcher.last_search().expect("report");
    assert_eq!(report.reason, SearchReason::NoCandidates);
    assert_eq!(report.best, None);
    assert_eq!(
        matcher.playback().map(|p| p.clip),
        playing.map(|p| p.clip),
        "keeps playing"
    );
    let mut fresh = MotionMatcher::new(eager());
    fresh.filter = matcher.filter;
    fresh.update(&db, &straight(1.5), DT).expect("update");
    assert!(fresh.last_search().is_some_and(|r| r.filter_ignored && r.jumped));
}

#[test]
fn the_plugin_applies_the_budget_staggers_and_sends_jump_events() {
    use regia_app::prelude::*;

    let db = Arc::new(build(&[walk(), run()]));
    let mut app = App::new();
    app.add_plugins(regia_app::MinimalPlugins);
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(20),
    ));
    app.add_plugins(MotionMatchPlugin);
    // A second install and a hand-registered system are harmless.
    app.add_plugins(MotionMatchPlugin);
    app.add_systems(Update, step_motion_matching);
    app.insert_resource(MotionMatchBudget {
        max_searches_per_frame: 1,
    });
    let settings = MatcherSettings {
        search_interval: 0.0,
        ..MatcherSettings::default()
    };
    let entities: Vec<Entity> = (0..3)
        .map(|_| {
            let mut m = MotionMatching::new(db.clone(), settings);
            m.trajectory = straight(4.0);
            app.world_mut().spawn(m).id()
        })
        .collect();
    let total = |app: &App| -> u64 {
        entities
            .iter()
            .map(|&e| {
                app.world()
                    .get::<MotionMatching>(e)
                    .map_or(0, |m| m.matcher.searches())
            })
            .sum()
    };
    app.update();
    // The first pick always runs: three searches and three jump events in the first frame.
    let first = total(&app);
    assert_eq!(first, 3);
    let messages = app.world().resource::<Messages<JumpEvent>>();
    let sent: Vec<Entity> = messages
        .iter_current_update_messages()
        .map(|m| m.entity)
        .collect();
    assert_eq!(sent.len(), 3);
    assert!(
        messages
            .iter_current_update_messages()
            .all(|m| m.from.is_none() && !m.mirrored)
    );
    for e in &entities {
        assert!(sent.contains(e));
    }
    let mut last = first;
    for _ in 0..12 {
        app.update();
        let now = total(&app);
        let fixed_steps = app.world().resource::<regia_app::FramePhases>().fixed_steps;
        assert!(
            now - last <= u64::from(fixed_steps),
            "at most one search per fixed step ({fixed_steps} this presentation frame)"
        );
        last = now;
    }
    for &e in &entities {
        let m = app.world().get::<MotionMatching>(e).expect("component");
        assert!(m.matcher.searches() >= 4, "every character keeps searching");
        assert!(m.matcher.deferrals() > 0);
    }
    // Stagger slots come from the entity index.
    for &e in &entities {
        let m = app.world().get::<MotionMatching>(e).expect("component");
        assert_eq!(u32::from(m.matcher.stagger_slot()), e.index_u32() % 8);
    }
}

/// ADR-4477 / audit animation1-determinism M8: equal fixed-step histories make equal matcher
/// progress even when one presentation frame contains two simulation ticks.
#[test]
fn the_plugin_budget_and_playback_follow_fixed_steps_not_presentation_frames() {
    use regia_app::prelude::*;

    fn run_schedule(
        frames: usize,
        presentation: std::time::Duration,
        sim_step: bool,
    ) -> (u32, Vec<(u64, Option<Playback>)>) {
        let db = Arc::new(build(&[walk(), run()]));
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, MotionMatchPlugin));
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(presentation));
        let mut fixed = regia_app::FixedTime::default();
        fixed.hz = 62.5; // 16 ms exactly: no nanosecond-rounding difference between the two runs.
        app.insert_resource(fixed);
        if sim_step {
            app.insert_resource(regia_app::SimStep(std::time::Duration::from_millis(16)));
        }
        app.insert_resource(MotionMatchBudget {
            max_searches_per_frame: 1,
        });
        let settings = MatcherSettings {
            search_interval: 0.0,
            ..MatcherSettings::default()
        };
        let entities: Vec<Entity> = (0..2)
            .map(|_| {
                let mut matching = MotionMatching::new(db.clone(), settings);
                matching.trajectory = straight(4.0);
                app.world_mut().spawn(matching).id()
            })
            .collect();
        let mut fixed_steps = 0;
        for _ in 0..frames {
            app.update();
            fixed_steps += app.world().resource::<regia_app::FramePhases>().fixed_steps;
        }
        let states = entities
            .into_iter()
            .map(|entity| {
                let matching = app.world().get::<MotionMatching>(entity).expect("matching");
                (matching.matcher.searches(), matching.matcher.playback())
            })
            .collect();
        (fixed_steps, states)
    }

    let fast = run_schedule(60, std::time::Duration::from_millis(16), false);
    let slow = run_schedule(30, std::time::Duration::from_millis(32), false);
    assert_eq!(fast.0, 60);
    assert_eq!(slow.0, 60);
    assert_eq!(fast.1, slow.1, "60 one-step frames vs 30 two-step frames");

    // A separate simulation clock, not the (much larger) presentation delta, drives playback.
    let ordinary = run_schedule(60, std::time::Duration::from_millis(16), false);
    let separated = run_schedule(60, std::time::Duration::from_millis(112), true);
    assert_eq!(separated.0, 60);
    assert_eq!(ordinary.1, separated.1);
}

// ---- debug view (§ 2.11) ------------------------------------------------------------------------

#[test]
fn the_candidate_report_explains_the_latest_search() {
    let db = tagged_db();
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        exclude_window: 0.2,
        ..MatcherSettings::default()
    });
    assert_eq!(matcher.candidate_report(&db, 8), None, "nothing searched yet");
    for _ in 0..6 {
        matcher.update(&db, &straight(1.5), DT).expect("update");
    }
    let report = matcher.candidate_report(&db, 8).expect("report");
    assert_eq!(report.rows.len(), 8);
    let last = matcher.last_search().expect("last");
    let continuation = report.continuation.expect("continuing");
    assert_eq!(continuation.status, CandidateStatus::Continuing);
    assert_eq!(Some(continuation.frame), matcher.playback().map(|p| p.frame));
    assert_eq!(
        continuation.parts.total.to_bits(),
        last.continuation_cost.expect("cost").to_bits()
    );
    for pair in report.rows.windows(2) {
        let key = |r: &CandidateRow| r.parts.total + r.parts.bias;
        assert!(key(&pair[0]) <= key(&pair[1]));
    }
    for row in &report.rows {
        let sum: f32 = row.parts.groups.iter().sum();
        close(
            sum,
            row.parts.total,
            1e-4 * row.parts.total.max(1.0),
            "group costs add up",
        );
        assert_eq!(row.transition, matcher.settings.transition_cost);
    }
    // The playing frame's neighbors are listed as near_playing; the best is the search's.
    assert!(
        report
            .rows
            .iter()
            .any(|r| r.status == CandidateStatus::NearPlaying)
    );
    if let Some(best) = last.best {
        assert!(
            report
                .rows
                .iter()
                .all(|r| r.status != CandidateStatus::Best || r.frame == best.frame)
        );
    }
    // A filter marks the other clip's frames as filtered.
    matcher.filter = SearchFilter {
        require: 0b10,
        forbid: 0,
    };
    matcher.update(&db, &straight(1.5), DT).expect("update");
    let report = matcher.candidate_report(&db, 32).expect("report");
    assert!(report.rows.iter().any(|r| r.status == CandidateStatus::Filtered));
    assert!(report.rows.iter().all(|r| {
        r.status != CandidateStatus::Filtered || db.frame_meta(r.frame).expect("m").tags & 0b10 == 0
    }));
}

// ---- player (§ 2.9, § 8 item 10) ----------------------------------------------------------------

fn sampled(clip: &AnimationClip, time: f32) -> Pose {
    let mut pose = skeleton().bind_pose_pose();
    clip.sample_pose(time, &mut pose);
    pose.translations[ROOT as usize].x = 0.0;
    pose.translations[ROOT as usize].z = 0.0;
    pose
}

fn max_translation_diff(a: &Pose, b: &Pose) -> f32 {
    a.translations
        .iter()
        .zip(&b.translations)
        .map(|(x, y)| (*x - *y).length())
        .fold(0.0, f32::max)
}

#[test]
fn the_player_blends_jumps_continuously_and_reports_root_motion() {
    let clips: Arc<[AnimationClip]> = Arc::from(vec![walk(), run()]);
    let db = build(&clips);
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        ..MatcherSettings::default()
    });
    let mut player = MotionMatchPlayer::new(clips.clone(), Arc::new(skeleton()));
    let mut jump_frame = None;
    let mut previous_output: Option<Pose> = None;
    for i in 0..90 {
        let speed = if i < 30 { 1.5 } else { 4.0 };
        let step = matcher.update(&db, &straight(speed), DT).expect("update");
        player.advance(&db, &matcher, &step, DT);
        let delta = player.root_motion();
        assert!(delta.translation.is_finite() && delta.yaw.is_finite());
        if i > 0 {
            // Root motion is the playing clip's speed over one frame, jump frames included.
            let clip_speed = if step.playback.clip == 0 { 1.5 } else { 4.0 };
            close(delta.translation.z, clip_speed * DT, 2e-3, "root delta");
        }
        if i > 0 && step.jumped && jump_frame.is_none() {
            let jump = step.jump.expect("jump");
            let from = jump.from.expect("from");
            jump_frame = Some(i);
            // On the jump frame the output equals the pose the old clip would have shown.
            let source = sampled(&clips[from.clip], from.time);
            assert!(
                max_translation_diff(player.pose(), &source) <= 1e-4,
                "pose continuity at the jump: {}",
                max_translation_diff(player.pose(), &source)
            );
            // ... which is within one frame of the previous output.
            let previous = previous_output.as_ref().expect("previous");
            let one_frame = max_translation_diff(&sampled(&clips[from.clip], from.time - DT), &source);
            assert!(max_translation_diff(player.pose(), previous) <= one_frame + 1e-4);
            assert!(player.is_blending());
        }
        previous_output = Some(player.pose().clone());
    }
    assert!(jump_frame.is_some(), "the run path jumped to the run");
    // Long after the blend the output is the target clip's pose.
    let playing = matcher.playback().expect("playing");
    assert!(!player.is_blending());
    let target = sampled(&clips[playing.clip], playing.time);
    assert!(max_translation_diff(player.pose(), &target) <= 1e-4);
    let (duration, _) = player.last_blend().expect("blend");
    assert!((player.duration_min..=player.duration_max).contains(&duration));
    assert!(db.median_jump_cost() > 0.0);
}

/// ADR-4477 / audit animation1-determinism H3: root translation is expressed with the same
/// portable yaw quaternion on every host, not `Quat::from_rotation_y`'s platform trig.
#[test]
fn turning_root_motion_uses_the_portable_yaw_oracle() {
    let angle = f32::from_bits(0x3edf_285b);
    let before_yaw = 2.0 * angle;
    let turn = Quat::from_rotation_y(before_yaw);
    let times = vec![0.0, 0.5, 0.75, 1.0];
    let mut clip = AnimationClip::empty("root_turn", 1.0);
    clip.tracks.push(JointTrack {
        joint: ROOT,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: vec![Vec3::ZERO, Vec3::ZERO, Vec3::X, Vec3::X],
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times,
            values: vec![Quat::IDENTITY, turn, turn, turn],
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let clips: Arc<[AnimationClip]> = Arc::from(vec![clip]);
    let db = MotionDatabase::build(
        &skeleton(),
        &[MotionClip {
            clip: &clips[0],
            looping: false,
        }],
        schema(),
    )
    .expect("database");
    let playback = Playback {
        clip: 0,
        time: 0.75,
        frame: db.frame_at(0, 0.75).expect("frame"),
    };
    let step = MatchStep {
        playback,
        searched: false,
        jumped: false,
        jump: None,
        deferred: false,
    };
    let matcher = MotionMatcher::new(MatcherSettings::default());
    let mut player = MotionMatchPlayer::new(clips.clone(), Arc::new(skeleton()));
    player.advance(&db, &matcher, &step, 0.25);

    let mut extractor = Extractor::new(&player.skeleton, db.schema());
    let source = MotionClip {
        clip: &clips[0],
        looping: false,
    };
    let now = extractor.sample(source, 0.75);
    let before = extractor.sample(source, 0.5);
    let expected = crate::num::det_rotation_y(-before.yaw) * (now.root - before.root);
    let actual = player.root_motion();
    assert_eq!(
        actual.translation.to_array().map(f32::to_bits),
        expected.to_array().map(f32::to_bits)
    );
    assert_eq!(actual.yaw.to_bits(), (now.yaw - before.yaw).to_bits());
}

#[test]
fn stride_rate_warp_speeds_playback_up_to_its_clamp() {
    let clips: Arc<[AnimationClip]> = Arc::from(vec![walk()]);
    let db = Arc::new(build(&clips));
    let mut matching = MotionMatching::new(db.clone(), MatcherSettings::default());
    matching.trajectory = straight(1.5);
    let mut player = MotionMatchPlayer::new(clips, Arc::new(skeleton()));
    player.warps.stride = warp::StrideWarpSettings {
        strength: 1.0,
        mode: warp::StrideMode::Rate,
        ..warp::StrideWarpSettings::default()
    };
    // Twice the clip's speed: the ratio is clamped to 1.25 and smoothed toward it.
    player.warp_inputs.actual_speed = Some(3.0);
    let mut last = 1.0;
    for _ in 0..60 {
        matching.step(DT).expect("step");
        step_player(&mut matching, &mut player, DT);
        assert!(player.stride_ratio() >= last - 1e-6, "monotone approach");
        assert!(player.stride_ratio() <= 1.25 + 1e-6);
        last = player.stride_ratio();
    }
    close(player.stride_ratio(), 1.25, 1e-3, "settled at the clamp");
    close(
        matching.matcher.rate_scale,
        player.stride_ratio(),
        1e-6,
        "rate handed to the matcher",
    );
    let before = matching.matcher.playback().expect("p").time;
    matching.step(DT).expect("step");
    let after = matching.matcher.playback().expect("p").time;
    close(
        (after - before).rem_euclid(1.0),
        1.25 * DT,
        2e-3,
        "playback runs 1.25x",
    );
    // Below the minimum speed the warp is off.
    player.warp_inputs.actual_speed = Some(0.05);
    for _ in 0..90 {
        matching.step(DT).expect("step");
        step_player(&mut matching, &mut player, DT);
    }
    close(player.stride_ratio(), 1.0, 1e-3, "off below min_speed");
}

/// A stop: 2 m/s braking at 2 m/s² (stopped after 1 s, 1 m traveled), then 0.5 s standing.
pub(super) fn stop_clip() -> AnimationClip {
    let times: Vec<f32> = (0..=90).map(|k| k as f32 / 60.0).collect();
    let z = |t: f32| {
        let t = t.min(1.0);
        2.0 * t - t * t
    };
    let mut clip = AnimationClip::empty("stop", 1.5);
    clip.tracks.push(JointTrack {
        joint: ROOT,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(0.0, 0.0, z(t))).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip
}

#[test]
fn distance_matching_places_the_stop_by_the_remaining_distance() {
    let clips: Arc<[AnimationClip]> = Arc::from(vec![stop_clip()]);
    let db = Arc::new(
        MotionDatabase::build(
            &skeleton(),
            &[MotionClip {
                clip: &clips[0],
                looping: false,
            }],
            schema(),
        )
        .expect("db"),
    );
    let mut matching = MotionMatching::new(db.clone(), MatcherSettings::default());
    let mut player = MotionMatchPlayer::new(clips, Arc::new(skeleton()));
    player.warps.distance_clips = vec![0];
    let total = player.arc_table(&db, 0).expect("table").total();
    close(total, 1.0, 2e-3, "arc length of the stop");
    for (distance, remaining) in [(1.0, 1.0), (0.5, 0.5), (0.0, 0.0)] {
        player.warp_inputs.distance_to_target = Some(distance);
        matching.step(DT).expect("step");
        step_player(&mut matching, &mut player, DT);
        let time = matching.matcher.playback().expect("p").time;
        let left = player.arc_table(&db, 0).expect("table").remaining_at(time);
        close(left, remaining, 1e-3, "remaining path at the chosen time");
    }
    // The target moving away holds playback.
    let held = matching.matcher.playback().expect("p").time;
    player.warp_inputs.distance_to_target = Some(0.8);
    matching.step(0.0).expect("step");
    step_player(&mut matching, &mut player, 0.0);
    close(matching.matcher.playback().expect("p").time, held, 1e-6, "held");
}

#[test]
fn orientation_warp_turns_the_root_by_the_clipped_heading_change() {
    let clips: Arc<[AnimationClip]> = Arc::from(vec![walk()]);
    let db = Arc::new(build(&clips));
    let mut matching = MotionMatching::new(db, MatcherSettings::default());
    matching.trajectory = straight(1.5);
    let mut player = MotionMatchPlayer::new(clips, Arc::new(skeleton()));
    player.warps.orientation.strength = 1.0;
    player.warp_inputs.desired_heading = Some(2.5);
    matching.step(DT).expect("step");
    step_player(&mut matching, &mut player, DT);
    close(
        player.orientation_delta(),
        std::f32::consts::FRAC_PI_2,
        1e-4,
        "clipped to 90 degrees",
    );
    let (axis, angle) = player.pose().rotations[ROOT as usize].to_axis_angle();
    close(angle, std::f32::consts::FRAC_PI_2, 1e-3, "root turned");
    assert!(axis.y.abs() > 0.99);
}

// ---- files (§ 4, § 8 item 12) -------------------------------------------------------------------

fn annotated_history_db() -> MotionDatabase {
    let clips = circle_clips();
    let mut schema = history_schema();
    schema.sample_weights.trajectory = vec![2.0, 1.0, 1.0];
    let mut db = MotionDatabase::build(&skeleton(), &looping(&clips), schema).expect("db");
    db.annotate(
        tag_table(),
        &[
            ClipAnnotation {
                tags: 0b01,
                spans: vec![Span {
                    start: 0.0,
                    end: 0.3,
                    kind: SpanKind::LandingBias(-0.5),
                }],
            },
            ClipAnnotation {
                tags: 0b10,
                spans: vec![Span {
                    start: 1.0,
                    end: 1.5,
                    kind: SpanKind::Unindexed,
                }],
            },
            ClipAnnotation::default(),
        ],
    )
    .expect("annotate");
    db
}

/// ADR-4477 / audit animation1-determinism H1: the trajectory floats serialized in the cooked
/// database come from the portable series, including their content hash and normalized rows.
#[test]
fn cooked_turn_features_use_the_portable_trig_oracle() {
    let angle = f32::from_bits(0x3edf_285b);
    let times = vec![0.0, 1.0 / 3.0, 1.0];
    let turn = Quat::from_rotation_y(angle);
    let mut clip = AnimationClip::empty("portable_turn", 1.0);
    clip.tracks.push(JointTrack {
        joint: ROOT,
        translations: TrackCurve::Static(Vec3::ZERO),
        rotations: TrackCurve::Animated {
            times,
            values: vec![Quat::IDENTITY, turn, turn],
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let schema = FeatureSchema::new(ROOT, Vec::new());
    let db = MotionDatabase::build(
        &skeleton(),
        &[MotionClip {
            clip: &clip,
            looping: false,
        }],
        schema,
    )
    .expect("database");
    let raw = db.raw_features(0).expect("first frame");
    let directions = db.schema().group_range(FeatureGroup::TrajectoryDirection);
    let (sin, cos) = crate::num::det_sin_cos(angle);
    assert_eq!(raw[directions.start].to_bits(), sin.to_bits());
    assert_eq!(raw[directions.start + 1].to_bits(), cos.to_bits());
    assert_eq!(raw[directions.start].to_bits(), 0x3ed8_28b0);

    let cooked = db.to_cooked_payload().expect("cook");
    let loaded = MotionDatabase::from_cooked_payload(&cooked).expect("load");
    assert_eq!(loaded.content_hash(), db.content_hash());
    assert_eq!(loaded.raw_features(0), db.raw_features(0));
}

#[test]
fn cooking_is_deterministic_and_round_trips_exactly() {
    let a = annotated_history_db().to_cooked_payload().expect("cook");
    let b = annotated_history_db().to_cooked_payload().expect("cook");
    assert_eq!(a, b, "two builds of the same inputs cook to the same bytes");
    let original = annotated_history_db();
    let loaded = MotionDatabase::from_cooked_payload(&a).expect("load");
    assert_eq!(loaded.len(), original.len());
    assert_eq!(loaded.content_hash(), original.content_hash());
    let hash = u64::from_le_bytes(a[a.len() - 8..].try_into().expect("8 bytes"));
    assert_eq!(
        hash,
        original.content_hash(),
        "the stored hash is the content hash"
    );
    for frame in 0..original.len() {
        let x = original.features(frame).expect("f");
        let y = loaded.features(frame).expect("f");
        assert!(
            x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits()),
            "frame {frame}"
        );
        assert_eq!(original.frame_meta(frame), loaded.frame_meta(frame));
    }
    assert_eq!(loaded.tag_table(), original.tag_table());
    assert_eq!(loaded.kd_stats(), original.kd_stats());
    for speed in [0.5, 1.5, 3.0] {
        let q = original
            .normalize(&original.query_with_mean_pose(&straight(speed)))
            .expect("q");
        for filter in [
            SearchFilter::NONE,
            SearchFilter {
                require: 0b10,
                forbid: 0,
            },
        ] {
            for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
                let o = original.search_detailed(&q, Exclusion::NONE, filter, 0.0, |_| false, method);
                let l = loaded.search_detailed(&q, Exclusion::NONE, filter, 0.0, |_| false, method);
                assert_eq!(
                    o.hit.map(|h| (h.frame, h.cost.to_bits())),
                    l.hit.map(|h| (h.frame, h.cost.to_bits()))
                );
            }
        }
    }
}

#[test]
fn a_damaged_or_newer_cooked_payload_is_refused() {
    let bytes = annotated_history_db().to_cooked_payload().expect("cook");
    for at in [0, 1, bytes.len() / 2, bytes.len() - 9, bytes.len() - 1] {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x10;
        assert!(
            matches!(
                MotionDatabase::from_cooked_payload(&damaged),
                Err(MotionMatchError::CorruptDatabase(_))
            ),
            "flipped byte {at}"
        );
    }
    assert!(matches!(
        MotionDatabase::from_cooked_payload(&bytes[..5]),
        Err(MotionMatchError::CorruptDatabase(_))
    ));
    // A payload one version newer than supported, with a valid hash: refused by version, not
    // as corrupt (ADR-2858 moved the current version from 1 to 2).
    let mut body = bytes[..bytes.len() - 8].to_vec();
    let current = COOKED_MOTIONDB_VERSION as u8;
    assert_eq!(
        body[0], current,
        "format_version is the first field (one-byte varint)"
    );
    body[0] = current + 1;
    let mut hash = Fnv1a::new();
    hash.write(&body);
    body.extend_from_slice(&hash.finish().to_le_bytes());
    assert_eq!(
        MotionDatabase::cooked_payload_version(&body),
        Ok(COOKED_MOTIONDB_VERSION + 1)
    );
    assert_eq!(
        MotionDatabase::from_cooked_payload(&body).err(),
        Some(MotionMatchError::UnsupportedFormatVersion {
            found: COOKED_MOTIONDB_VERSION + 1,
            supported: COOKED_MOTIONDB_VERSION
        })
    );
}

#[test]
fn the_cooked_container_carries_the_database_type_tag() {
    use regia_asset::{
        AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version,
    };
    let db = annotated_history_db();
    let payload = db.to_cooked_payload().expect("cook");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: crate::cooked::motion_database_type_tag(),
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(0, 2, 5),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, &payload).expect("encode");
    let (_, loaded) = crate::cooked::load_cooked_motion_database(&file).expect("load");
    assert_eq!(loaded.content_hash(), db.content_hash());
    let wrong = CookedHeader {
        type_tag: crate::cooked::clip_type_tag(),
        ..header
    };
    let file = encode_cooked_file(&wrong, &payload).expect("encode");
    assert!(matches!(
        crate::cooked::load_cooked_motion_database(&file),
        Err(crate::cooked::MotionDatabaseLoadError::Asset(_))
    ));
}

pub(super) const SOURCE: &str = r#"(
  format_version: 1,
  skeleton: "characters/test.skeleton",
  schema: ( root_joint: "root", forward: (0.0, 0.0, 1.0), feet: ["foot_l", "foot_r"],
            trajectory_times: [0.333, 0.667, 1.0], history_times: [0.25, 0.5],
            sample_rate: 30.0, weights: ( foot_velocity: 0.5 ), normalization: group_deviation,
            pose_joints: [] ),
  tags: ["walk", "run", "stop"],
  entries: [
    ( clip: "anim/walk.clip", looping: true, tags: ["walk"],
      spans: [ ( kind: landing_bias(-0.5), range: (0.0, 0.2) ), ( kind: tag(["stop"]), range: (0.5, 0.6) ) ] ),
    ( clip: "anim/run.clip", looping: true, tags: ["run"], trim: (0.1, 0.2) ),
    ( clip: "anim/stop.clip", tags: ["stop"], distance_matched: true ),
  ],
)"#;

pub(super) fn source_clips() -> Vec<(String, AnimationClip)> {
    vec![
        ("anim/walk.clip".into(), walk()),
        ("anim/run.clip".into(), run()),
        ("anim/stop.clip".into(), stop_clip()),
    ]
}

#[test]
fn a_motiondb_source_builds_the_annotated_database() {
    let source = MotionDatabaseSource::from_ron_str(SOURCE).expect("parse");
    let clips = source_clips();
    let lookup = |path: &str| clips.iter().find(|(p, _)| p == path).map(|(_, c)| c);
    let db = source.build(&skeleton(), lookup).expect("build");
    assert_eq!(db.clips().len(), 3);
    assert!(!db.clips()[2].looping && db.clips()[2].distance_matched);
    assert_eq!(db.schema().history_times, vec![0.25, 0.5]);
    assert_eq!(db.schema().weights.foot_velocity, 0.5);
    assert_eq!(db.tag_table().names(), ["walk", "run", "stop"]);
    let walk0 = db.frame_meta(0).expect("meta");
    assert_eq!((walk0.tags, walk0.bias), (0b001, -0.5));
    let tagged = db.frame_meta(db.frame_at(0, 0.5).expect("f")).expect("meta");
    assert_eq!(tagged.tags, 0b101, "the tag span adds stop");
    // The trims make the run's ends pass-through: played, never targets.
    let run = &db.clips()[1];
    assert!(!db.frame_meta(run.start).expect("m").is_target());
    assert!(!db.frame_meta(run.start + run.frames - 1).expect("m").is_target());
    assert!(db.frame_meta(run.start + 10).expect("m").is_target());
    // Writing and reading the source again changes nothing.
    let text = source.to_ron_string().expect("write");
    assert_eq!(MotionDatabaseSource::from_ron_str(&text).expect("parse"), source);
    // The same source cooks to the same bytes as a hand-built database would.
    let again = source.build(&skeleton(), lookup).expect("build");
    assert_eq!(
        again.to_cooked_payload().expect("c"),
        db.to_cooked_payload().expect("c")
    );
}

#[test]
fn motiondb_source_errors_name_the_problem() {
    let clips = source_clips();
    let lookup = |path: &str| clips.iter().find(|(p, _)| p == path).map(|(_, c)| c);
    let v2 = SOURCE.replace("format_version: 1", "format_version: 2");
    let parsed = MotionDatabaseSource::from_ron_str(&v2).expect("format_version 2 parses");
    assert_eq!(parsed.format_version, MOTIONDB_FORMAT_VERSION);
    parsed
        .build(&skeleton(), lookup)
        .expect("format_version 2 builds");
    let edit = |from: &str, to: &str| {
        let text = SOURCE.replace(from, to);
        MotionDatabaseSource::from_ron_str(&text).and_then(|s| s.build(&skeleton(), lookup))
    };
    assert_eq!(
        edit("format_version: 1", "format_version: 3").err(),
        Some(MotionMatchError::UnsupportedFormatVersion {
            found: 3,
            supported: MOTIONDB_FORMAT_VERSION
        })
    );
    assert_eq!(
        edit("root_joint: \"root\"", "root_joint: \"pelvis\"").err(),
        Some(MotionMatchError::UnknownJoint("pelvis".into()))
    );
    assert_eq!(
        edit("anim/run.clip\", looping", "anim/sprint.clip\", looping").err(),
        Some(MotionMatchError::UnknownClip {
            entry: 1,
            clip: "anim/sprint.clip".into()
        })
    );
    assert_eq!(
        edit("tags: [\"run\"]", "tags: [\"jog\"]").err(),
        Some(MotionMatchError::UnknownTag {
            entry: 1,
            tag: "jog".into()
        })
    );
    assert_eq!(
        edit("tags: [\"run\"], trim", "tags: [\"run\"], mirrored: true, trim").err(),
        Some(MotionMatchError::NoMirrorTable { entry: 1 })
    );
    assert!(matches!(
        edit("range: (0.0, 0.2)", "range: (0.4, 0.2)").err(),
        Some(MotionMatchError::BadSpan { entry: 0, .. })
    ));
    assert!(matches!(
        edit("entries:", "entries"),
        Err(MotionMatchError::BadSource(_))
    ));
}

#[test]
fn a_clip_whose_feet_stand_still_while_the_root_moves_gets_a_warning() {
    // The root moves 1 m/s but the feet are keyed against it, so in object space they stand
    // still: the usual sign of root motion baked into the wrong joint.
    let times: Vec<f32> = (0..=60).map(|k| k as f32 / 60.0).collect();
    let mut clip = AnimationClip::empty("skate", 1.0);
    clip.tracks.push(JointTrack {
        joint: ROOT,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(0.0, 0.0, t)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    for (joint, x) in [(FOOT_L, -0.1f32), (FOOT_R, 0.1)] {
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times.iter().map(|&t| Vec3::new(x, 0.0, -t)).collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    let clips = [walk(), clip];
    let db = MotionDatabase::build(
        &skeleton(),
        &[
            MotionClip {
                clip: &clips[0],
                looping: true,
            },
            MotionClip {
                clip: &clips[1],
                looping: false,
            },
        ],
        schema(),
    )
    .expect("db");
    assert_eq!(
        db.warnings(),
        &[BuildWarning {
            clip: 1,
            kind: BuildWarningKind::MissingRootMotion
        }]
    );
    let loaded = MotionDatabase::from_cooked_payload(&db.to_cooked_payload().expect("c")).expect("l");
    assert_eq!(
        loaded.warnings(),
        db.warnings(),
        "warnings survive the cooked file"
    );
}

// ---- mirrored entries (§ 2.4) -------------------------------------------------------------------

fn mirror() -> MirrorTable {
    MirrorTable {
        pairs: vec![(FOOT_L, FOOT_R)],
    }
}

#[test]
fn a_mirrored_turn_is_the_other_turn_half_a_stride_later() {
    let clips = circle_clips();
    let entries = [
        MotionEntry {
            clip: &clips[1],
            looping: true,
            mirrored: true,
        },
        MotionEntry {
            clip: &clips[2],
            looping: true,
            mirrored: false,
        },
    ];
    let db = MotionDatabase::build_entries(&skeleton(), &entries, schema(), Some(&mirror())).expect("db");
    assert_eq!(db.clips().len(), 3);
    assert_eq!(db.clips()[1].name, "turn_pos.mirrored");
    assert!(db.clips()[1].mirrored && !db.clips()[0].mirrored);
    assert_eq!((db.clips()[1].source, db.clips()[2].source), (0, 1));
    assert_eq!(db.mirror_partners(), &[0, 2, 1]);
    // Mirroring swaps the feet (whose swing is in opposite phase) and turns the other way: the
    // mirrored turn_pos at frame k is turn_neg at frame k + 15 (half a 1 s stride at 30 Hz).
    let (mirrored, negative) = (db.clips()[1].start, db.clips()[2].start);
    for k in [0, 7, 20, 44] {
        let a = db.raw_features(mirrored + k).expect("raw");
        let b = db.raw_features(negative + (k + 15) % 60).expect("raw");
        for (d, (x, y)) in a.iter().zip(b).enumerate() {
            assert!((x - y).abs() < 2e-3, "frame {k} dim {d}: {x} vs {y}");
        }
    }
    // No table: refused, naming the entry.
    assert_eq!(
        MotionDatabase::build_entries(&skeleton(), &entries, schema(), None).err(),
        Some(MotionMatchError::NoMirrorTable { entry: 0 })
    );
    let bad = MirrorTable {
        pairs: vec![(FOOT_L, FOOT_R), (FOOT_R, ROOT)],
    };
    assert!(matches!(
        MotionDatabase::build_entries(&skeleton(), &entries, schema(), Some(&bad)),
        Err(MotionMatchError::BadMirrorTable(_))
    ));
    // The cooked file keeps the mirrored ranges and the partners.
    let loaded = MotionDatabase::from_cooked_payload(&db.to_cooked_payload().expect("c")).expect("l");
    assert_eq!(loaded.mirror_partners(), db.mirror_partners());
    assert_eq!(loaded.clips(), db.clips());
}

#[test]
fn the_player_mirrors_the_pose_and_the_root_motion() {
    let clips: Arc<[AnimationClip]> = Arc::from(vec![locomotion("turn_pos", 1.5, 0.75, 0.3, 1.0, 2.0)]);
    let entries = [MotionEntry {
        clip: &clips[0],
        looping: true,
        mirrored: true,
    }];
    let db = Arc::new(
        MotionDatabase::build_entries(&skeleton(), &entries, schema(), Some(&mirror())).expect("db"),
    );
    let mut player = MotionMatchPlayer::new(clips.clone(), Arc::new(skeleton()));
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    // Ask for the mirrored turn's path: the first pick lands in the mirrored range.
    let path = db.frame_query(db.clips()[1].start + 10).expect("q").trajectory;
    let step = matcher.update(&db, &path, DT).expect("update");
    assert_eq!(step.playback.clip, 1, "the mirrored range");
    player.advance(&db, &matcher, &step, DT);
    let step = matcher.update(&db, &path, DT).expect("update");
    player.advance(&db, &matcher, &step, DT);
    // The left foot shows the right foot's reflected local transform.
    let mut plain = skeleton().bind_pose_pose();
    clips[0].sample_pose(step.playback.time, &mut plain);
    let left = player.pose().translations[FOOT_L as usize];
    let right = plain.translations[FOOT_R as usize];
    close3(left, Vec3::new(-right.x, right.y, right.z), 1e-5, "mirrored foot");
    // The root turns the other way: negative yaw and a leftward (negative x) drift.
    let delta = player.root_motion();
    assert!(delta.yaw < 0.0, "yaw {}", delta.yaw);
    close(delta.yaw, -0.75 * DT, 1e-3, "mirrored yaw rate");
    assert!(delta.translation.x <= 1e-6, "x {}", delta.translation.x);
}

#[test]
fn a_motiondb_source_with_mirror_pairs_builds_mirrored_entries() {
    let text = SOURCE
        .replace(
            "tags: [\"walk\", \"run\", \"stop\"],",
            "tags: [\"walk\", \"run\", \"stop\"], mirror_pairs: [(\"foot_l\", \"foot_r\")],",
        )
        .replace("tags: [\"run\"], trim", "tags: [\"run\"], mirrored: true, trim");
    let source = MotionDatabaseSource::from_ron_str(&text).expect("parse");
    let clips = source_clips();
    let lookup = |path: &str| clips.iter().find(|(p, _)| p == path).map(|(_, c)| c);
    let db = source.build(&skeleton(), lookup).expect("build");
    let names: Vec<&str> = db.clips().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["walk", "run", "run.mirrored", "stop"]);
    let mirrored = &db.clips()[2];
    assert_eq!(mirrored.source, 1);
    let meta = db.frame_meta(mirrored.start + 10).expect("meta");
    assert_eq!(meta.tags, 0b010, "the mirrored copy keeps the entry's tags");
    assert!(meta.flags & FLAG_MIRRORED != 0);
    assert!(
        !db.frame_meta(mirrored.start).expect("m").is_target(),
        "and its trim"
    );
    assert!(db.clips()[3].distance_matched);
}

// ---- hand-run budgets (§ 10; docs/06-testing-and-ci.md § 9) -------------------------------------

/// `rows` × 48 floats on a smooth low-dimensional manifold (a few phases and speeds through sines),
/// closer to motion data than uniform noise, with a little noise on top.
fn manifold_rows(rng: &mut Rng, rows: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(rows * 48);
    for r in 0..rows {
        let phase = r as f32 * 0.037;
        let speed = (r / 600) as f32 * 0.21 % 4.0;
        let turn = ((r / 1700) as f32 * 0.7).sin();
        for d in 0..48 {
            let k = d as f32;
            let v = (phase * (1.0 + k * 0.13)).sin() * (0.5 + 0.1 * speed)
                + speed * (k * 0.07).cos()
                + turn * (k * 0.31).sin();
            out.push(v + (rng.f32() - 0.5) * 0.02);
        }
    }
    out
}

#[test]
#[ignore = "hand-run timing; run in release with --ignored --nocapture"]
fn perf_micro_spec52_budgets() {
    use std::time::Instant;

    let mut rng = Rng::new(0x052b);
    for rows in [10_000usize, 100_000, 200_000] {
        let data = manifold_rows(&mut rng, rows);
        let started = Instant::now();
        let tree = KdTree::build(&data, 48).expect("tree");
        let build_ms = started.elapsed().as_secs_f64() * 1e3;
        let queries: Vec<Vec<f32>> = (0..300)
            .map(|_| {
                let row = (rng.next_u32() as usize) % rows;
                data[row * 48..(row + 1) * 48]
                    .iter()
                    .map(|v| v + (rng.f32() - 0.5) * 0.3)
                    .collect()
            })
            .collect();
        let report = |name: &str, f: &dyn Fn(&[f32]) -> Option<Nearest>| {
            let times: Vec<f64> = queries
                .iter()
                .map(|q| {
                    let started = Instant::now();
                    let hit = f(q);
                    let us = started.elapsed().as_secs_f64() * 1e6;
                    assert!(hit.is_some());
                    us
                })
                .collect();
            let (p50, p99, max) = percentiles(times);
            println!(
                "perf-micro spec52_search_{name}: frames={rows} dims=48 p50={p50:.1}us p99={p99:.1}us max={max:.1}us"
            );
        };
        report("brute_force", &|q| nearest_brute_force(&data, 48, q, |_| false));
        report("kd_tree", &|q| tree.nearest(q, |_| false));
        println!(
            "perf-micro spec52_tree_build: frames={rows} ms={build_ms:.1} stats={:?}",
            tree.stats()
        );
    }

    // Cooked load of a clip database (recompute the normalized rows, verify, rebuild the tree).
    let clips: Vec<AnimationClip> = (0..20)
        .map(|i| locomotion(&format!("clip{i}"), 0.5 + 0.2 * i as f32, 0.0, 0.3, 1.0, 60.0))
        .collect();
    let started = Instant::now();
    let db = build(&clips);
    let build_ms = started.elapsed().as_secs_f64() * 1e3;
    let bytes = db.to_cooked_payload().expect("cook");
    let started = Instant::now();
    let loaded = MotionDatabase::from_cooked_payload(&bytes).expect("load");
    let load_ms = started.elapsed().as_secs_f64() * 1e3;
    println!(
        "perf-micro spec52_cooked: frames={} bytes={} build_ms={build_ms:.1} load_ms={load_ms:.1}",
        loaded.len(),
        bytes.len()
    );
}

#[test]
fn matcher_settings_round_trip_through_ron_with_defaults() {
    let settings = MatcherSettings {
        method: SearchMethod::KdTree,
        urgent_threshold: Some(0.75),
        stagger: false,
        play_rate: 1.25,
        ..MatcherSettings::default()
    };
    let text = ron::to_string(&settings).expect("write");
    assert_eq!(ron::from_str::<MatcherSettings>(&text).expect("read"), settings);
    let partial: MatcherSettings = ron::from_str("(method: auto, transition_cost: 0.5)").expect("partial");
    assert_eq!(
        partial,
        MatcherSettings {
            transition_cost: 0.5,
            ..MatcherSettings::default()
        }
    );
}

#[test]
fn the_component_keeps_a_report_only_when_asked() {
    let db = Arc::new(build(&[walk(), run()]));
    let mut quiet = MotionMatching::new(db.clone(), MatcherSettings::default());
    quiet.trajectory = straight(1.5);
    quiet.step(DT).expect("step");
    assert_eq!(quiet.last_report, None);
    let mut asked = MotionMatching::new(db, MatcherSettings::default());
    asked.trajectory = straight(1.5);
    asked.report_candidates = 5;
    asked.step(DT).expect("step");
    let report = asked.last_report.clone().expect("report");
    assert_eq!(report.rows.len(), 5);
    assert_eq!(
        report.method_used,
        Some(SearchMethod::BruteForce),
        "Auto on a small database"
    );
    assert!(report.rows.iter().any(|r| r.status == CandidateStatus::Best));
}
