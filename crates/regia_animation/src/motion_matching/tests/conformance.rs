// SPDX-License-Identifier: MPL-2.0
//! Spec 52 conformance tests (`docs/specs/52-motion-matching.md`): behavior the spec states that
//! was built but not asserted, found by the conformance audit (lane spec-20-52, ADR-5639): the
//! joint and dimension limits, no flicker over 600 fixed steps, a pass-through run-through, an
//! exact tie broken by a landing bias and the live-pose blend. Deterministic: fixed clips and
//! fixed steps, no wall clock.

use super::*;
use crate::inertialize::DecayMethod;

/// A skeleton of a root and `children` joints under it.
fn wide_skeleton(children: usize) -> Skeleton {
    let mut names = vec!["root".to_owned()];
    let mut parents = vec![None];
    let mut binds = vec![translated(Vec3::ZERO)];
    for i in 0..children {
        names.push(format!("j{i}"));
        parents.push(Some(ROOT));
        binds.push(translated(Vec3::new(0.01 * i as f32, 0.0, 0.0)));
    }
    Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton")
}

// § 2.2 limits and § 8.12 "schema validation errors each have a test": at most 64 joints named
// across `feet` and `pose_joints`, at most 192 floats per vector.
#[test]
fn too_many_joints_or_floats_are_typed_errors() {
    let skeleton = wide_skeleton(70);
    let joints = FeatureSchema::new(ROOT, (1..=65).collect());
    assert_eq!(
        joints.validate(&skeleton),
        Err(MotionMatchError::TooManyDimensions {
            what: "joints",
            got: 65,
            max: MAX_SCHEMA_JOINTS,
        })
    );
    assert_eq!(MAX_SCHEMA_JOINTS, 64);
    let mut at_limit = FeatureSchema::new(ROOT, (1..=20).collect());
    at_limit.trajectory_times = (1..=16).map(|i| i as f32 / 16.0).collect();
    at_limit.sample_weights.trajectory = vec![1.0; 16];
    // 3 + 16 * 2 * 2 + 20 feet * 6 = 187 floats: valid.
    assert_eq!(at_limit.dimensions(), 187);
    assert_eq!(at_limit.validate(&skeleton), Ok(()));
    let mut over = at_limit.clone();
    over.feet.push(21);
    // 193 floats with 21 joints: over the float cap, under the joint cap.
    assert_eq!(over.dimensions(), 193);
    assert_eq!(
        over.validate(&skeleton),
        Err(MotionMatchError::TooManyDimensions {
            what: "dimensions",
            got: 193,
            max: MAX_DIMENSIONS,
        })
    );
    assert_eq!(MAX_DIMENSIONS, 192);
}

// § 8.8: "a query sitting between two near-identical clips does not flicker over 600 fixed steps
// (`jumps` <= 2)", at the default settings.
#[test]
fn a_query_between_two_near_identical_clips_does_not_flicker_over_600_steps() {
    let db = build(&[
        locomotion("walk_a", 1.50, 0.0, 0.3, 1.0, 1.0),
        locomotion("walk_b", 1.54, 0.0, 0.3, 1.0, 1.0),
    ]);
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    let path = straight(1.52);
    for _ in 0..600 {
        matcher.update(&db, &path, DT).expect("update");
    }
    assert!(matcher.searches() > 100, "{} searches", matcher.searches());
    assert!(matcher.jumps() <= 2, "{} jumps over 600 steps", matcher.jumps());
}

// § 8.6: "`pass_through` frames never appear as candidates but the clip plays through them in a
// run-through test (no jump lands there over 10 s of fixed steps)".
#[test]
fn a_pass_through_span_is_played_through_but_never_landed_on() {
    let mut db = build(&[walk(), run()]);
    let span = (0.3_f32, 0.6_f32);
    db.annotate(
        TagTable::default(),
        &[
            ClipAnnotation {
                tags: 0,
                spans: vec![Span {
                    start: span.0,
                    end: span.1,
                    kind: SpanKind::PassThrough,
                }],
            },
            ClipAnnotation::default(),
        ],
    )
    .expect("annotate");
    let inside = |p: &Playback| {
        let frame = db.frame(p.frame).expect("frame");
        frame.clip == 0 && frame.time >= span.0 && frame.time < span.1
    };
    let eager = MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        transition_cost: 0.0,
        ..MatcherSettings::default()
    };
    let mut matcher = MotionMatcher::new(eager);
    let mut landed_inside = 0;
    let mut played_inside = 0;
    let mut jumps = 0;
    // 10 s at 30 Hz; the path switches between walking and running every second.
    for i in 0..300 {
        let speed = if (i / 30_u32).is_multiple_of(2) { 1.5 } else { 4.0 };
        let step = matcher.update(&db, &straight(speed), DT).expect("update");
        if step.jumped {
            jumps += 1;
            if inside(&step.playback) {
                landed_inside += 1;
            }
        } else if inside(&step.playback) {
            played_inside += 1;
        }
    }
    assert!(jumps >= 5, "the run-through must jump: {jumps}");
    assert_eq!(landed_inside, 0, "a jump landed in the pass-through span");
    assert!(played_inside > 0, "the walk never played through its span");
}

// § 8.6: "a `landing_bias(-b)` wins an otherwise exact tie and loses it for `+b`".
#[test]
fn a_landing_bias_breaks_an_exact_tie_both_ways() {
    let twin = || locomotion("walk", 1.5, 0.0, 0.3, 1.0, 1.0);
    let plain = build(&[twin(), twin()]);
    let q = walk_query(&plain);
    let tie = plain
        .search(&q, Exclusion::NONE, SearchMethod::BruteForce)
        .expect("hit");
    let (clip, local) = {
        let frame = plain.frame(tie.frame).expect("frame");
        (frame.clip, tie.frame - plain.clips()[frame.clip].start)
    };
    assert_eq!(clip, 0, "an exact tie goes to the lowest frame index");
    let twin_frame = plain.clips()[1].start + local;
    assert_eq!(
        plain.features(twin_frame),
        plain.features(tie.frame),
        "the clips are identical"
    );
    let biased = |clip: usize, b: f32| {
        let mut db = build(&[twin(), twin()]);
        let mut annotations = vec![ClipAnnotation::default(), ClipAnnotation::default()];
        annotations[clip].spans = vec![Span {
            start: 0.0,
            end: 10.0,
            kind: SpanKind::LandingBias(b),
        }];
        db.annotate(TagTable::default(), &annotations).expect("annotate");
        let brute = db
            .search_filtered(&q, SearchFilter::NONE, Exclusion::NONE, SearchMethod::BruteForce)
            .expect("hit");
        for method in [SearchMethod::KdTree, SearchMethod::Auto] {
            let hit = db
                .search_filtered(&q, SearchFilter::NONE, Exclusion::NONE, method)
                .expect("hit");
            assert_eq!(hit.frame, brute.frame, "{method:?}");
            assert_eq!(hit.cost.to_bits(), brute.cost.to_bits(), "{method:?}");
        }
        db.frame(brute.frame).expect("frame").clip
    };
    let b = 0.01;
    assert_eq!(biased(1, -b), 1, "-b on the second clip wins the tie");
    assert_eq!(biased(1, b), 0, "+b on the second clip keeps losing it");
    assert_eq!(biased(0, b), 1, "+b on the first clip loses the tie");
    assert_eq!(biased(0, -b), 0);
}

// § 2.6: pose features are the playing frame's, blended with the host's `LivePose` by
// `live_pose_blend` (a plain linear interpolation of the raw features); 0 is the baseline and
// the trajectory is always the game's path.
#[test]
fn live_pose_blend_interpolates_the_pose_features_only() {
    let db = build(&[walk(), run()]);
    let path = straight(1.5);
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    for _ in 0..5 {
        matcher.update(&db, &path, DT).expect("update");
    }
    let playing = matcher.playback().expect("playing").frame;
    let stored = db.query_from_frame(playing, &path).expect("stored");
    let live = LivePose {
        root_velocity: Vec3::new(0.5, 0.0, 3.0),
        feet: vec![
            FootFeature {
                position: Vec3::new(-0.2, 0.1, 0.4),
                velocity: Vec3::new(0.0, 0.5, 2.0),
            },
            FootFeature {
                position: Vec3::new(0.2, 0.0, -0.3),
                velocity: Vec3::new(0.0, 0.0, -1.0),
            },
        ],
        pose_joints: Vec::new(),
    };
    matcher.live_pose = Some(live.clone());
    // Blend 0 (the default) ignores the live pose: the baseline query.
    assert_eq!(matcher.current_query(&db, &path).expect("q"), stored);
    for blend in [0.25_f32, 0.5, 1.0] {
        matcher.settings.live_pose_blend = blend;
        let q = matcher.current_query(&db, &path).expect("q");
        assert_eq!(
            q.root_velocity,
            stored.root_velocity.lerp(live.root_velocity, blend)
        );
        for (i, foot) in q.feet.iter().enumerate() {
            assert_eq!(
                foot.position,
                stored.feet[i].position.lerp(live.feet[i].position, blend)
            );
            assert_eq!(
                foot.velocity,
                stored.feet[i].velocity.lerp(live.feet[i].velocity, blend)
            );
        }
        assert_eq!(q.trajectory, stored.trajectory, "the path is always the game's");
    }
    let full = matcher.current_query(&db, &path).expect("q");
    assert!(
        (full.root_velocity - live.root_velocity).length() < 1e-6,
        "blend 1 is the live pose"
    );
}

// § 2.11: the candidate report marks a pass-through frame `pass_through` (never `best`).
#[test]
fn the_candidate_report_marks_pass_through_frames() {
    let mut db = build(&[walk(), run()]);
    db.annotate(
        TagTable::default(),
        &[
            ClipAnnotation::default(),
            ClipAnnotation {
                tags: 0,
                spans: vec![Span {
                    start: 0.0,
                    end: 10.0,
                    kind: SpanKind::PassThrough,
                }],
            },
        ],
    )
    .expect("annotate");
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        ..MatcherSettings::default()
    });
    for _ in 0..4 {
        matcher.update(&db, &straight(1.5), DT).expect("update");
    }
    // A running path: the run frames are nearest but may never be landed on.
    let step = matcher.update(&db, &straight(4.0), DT).expect("update");
    assert_eq!(
        db.frame(step.playback.frame).expect("frame").clip,
        0,
        "still walking"
    );
    let report = matcher.candidate_report(&db, 16).expect("report");
    let run_rows: Vec<&CandidateRow> = report.rows.iter().filter(|r| r.clip == 1).collect();
    assert!(!run_rows.is_empty(), "the run frames are among the nearest");
    assert!(
        run_rows.iter().all(|r| r.status == CandidateStatus::PassThrough),
        "{:?}",
        run_rows.iter().map(|r| r.status).collect::<Vec<_>>()
    );
}

// § 2.11 with § 2.8 `lockout_window`: a frame chosen as a jump target within the window is
// reported `locked_out` (unless it is the playing frame's neighbor or the best).
#[test]
fn the_candidate_report_marks_recent_jump_targets_locked_out() {
    let db = build(&[
        locomotion("walk_a", 1.50, 0.0, 0.3, 1.0, 1.0),
        locomotion("walk_b", 1.54, 0.0, 0.3, 1.0, 1.0),
    ]);
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        transition_cost: 0.0,
        exclude_window: 0.0,
        lockout_window: 10.0,
        ..MatcherSettings::default()
    });
    for i in 0..8_u32 {
        let speed = if i.is_multiple_of(2) { 1.58 } else { 1.46 };
        matcher.update(&db, &straight(speed), DT).expect("update");
    }
    assert!(matcher.jumps() >= 2, "{} jumps", matcher.jumps());
    let report = matcher.candidate_report(&db, 32).expect("report");
    let locked: Vec<usize> = report
        .rows
        .iter()
        .filter(|r| r.status == CandidateStatus::LockedOut)
        .map(|r| r.frame)
        .collect();
    assert!(!locked.is_empty(), "a recent target is among the 32 nearest");
    for frame in locked {
        assert!(
            matcher.recent_targets.iter().any(|&(f, _)| f == frame),
            "frame {frame} is not a recent target"
        );
    }
}

// § 2.9: blend duration `lerp(duration_min, duration_max, saturate(jump_cost / duration_cost_ref))`
// with defaults 0.1 s, 0.25 s and the database's median candidate cost; a cross-clip jump uses
// the quintic decay.
#[test]
fn the_blend_duration_follows_the_jump_cost() {
    let clips: Arc<[AnimationClip]> = Arc::from(vec![walk(), run()]);
    let db = build(&clips);
    let mut player = MotionMatchPlayer::new(clips.clone(), Arc::new(skeleton()));
    assert_eq!((player.duration_min, player.duration_max), (0.1, 0.25));
    close(player.blend_duration(0.0, 2.0), 0.1, 1e-6, "zero cost");
    close(player.blend_duration(1.0, 2.0), 0.175, 1e-6, "half the reference");
    close(player.blend_duration(2.0, 2.0), 0.25, 1e-6, "the reference");
    close(player.blend_duration(9.0, 2.0), 0.25, 1e-6, "saturated");
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        ..MatcherSettings::default()
    });
    let mut checked = false;
    for i in 0..90 {
        let speed = if i < 30 { 1.5 } else { 4.0 };
        let step = matcher.update(&db, &straight(speed), DT).expect("update");
        player.advance(&db, &matcher, &step, DT);
        if let Some(jump) = step.jump
            && let Some(from) = jump.from
            && from.clip != jump.to.clip
        {
            let (duration, method) = player.last_blend().expect("a blend");
            assert_eq!(method, DecayMethod::Quintic, "a cross-clip jump");
            assert_eq!(
                duration.to_bits(),
                player.blend_duration(jump.cost, db.median_jump_cost()).to_bits()
            );
            checked = true;
        }
    }
    assert!(checked, "the run path jumped across clips");
}

// § 8.7: the random exactness sweep at 1,000 queries per database also for databases above 5,000
// rows (the spec52 sweep runs 200 there): brute force and the tree return the same row and a
// bit-identical biased cost, with duplicated rows (quantized data), ties and biases. `Auto` only
// ever runs one of these two exact methods (`auto_picks_the_tree_or_the_scan_...`).
#[test]
fn large_random_databases_agree_over_a_thousand_queries() {
    let mut rng = Rng::new(0x0052_8007);
    for (index, &(rows, dims)) in [(6000, 31), (8000, 48), (10_000, 27), (15_000, 64), (20_000, 96)]
        .iter()
        .enumerate()
    {
        let quantize = index.is_multiple_of(2);
        let data = random_rows(&mut rng, rows, dims, quantize);
        let biases: Vec<f32> = (0..rows)
            .map(|_| if rng.chance(0.3) { rng.f32() - 0.5 } else { 0.0 })
            .collect();
        let min_bias = biases.iter().copied().fold(0.0f32, f32::min);
        let tree = KdTree::build_with_leaf_size(&data, dims, 8).expect("tree");
        for q in 0..1000_u32 {
            let query: Vec<f32> = if q.is_multiple_of(3) {
                let row = (rng.next_u32() as usize) % rows;
                data[row * dims..(row + 1) * dims].to_vec()
            } else {
                random_rows(&mut rng, 1, dims, quantize)
            };
            let skip = (rng.next_u32() as usize) % rows;
            let excluded = |row: usize| row % 97 == skip % 97;
            let bias = |row: usize| biases[row];
            let (brute, _) = nearest_brute_force_biased_counted(&data, dims, &query, excluded, bias);
            let (tree_hit, _) = tree.nearest_biased_counted(&query, excluded, bias, min_bias);
            assert_eq!(
                brute.map(|n| (n.row, n.distance.to_bits())),
                tree_hit.map(|n| (n.row, n.distance.to_bits())),
                "rows {rows} dims {dims} q {q}"
            );
        }
    }
}
