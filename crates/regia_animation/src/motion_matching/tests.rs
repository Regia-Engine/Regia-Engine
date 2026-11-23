// SPDX-License-Identifier: MPL-2.0
//! Motion-matching tests: feature extraction, normalization, exact search, and the runtime
//! matcher's transition cost and minimum time between jumps. Everything is deterministic: fixed
//! clips, a seeded PCG32 for the random data, fixed time steps.

use std::f32::consts::TAU;
use std::sync::Arc;

use regia_core::math::{Quat, Rng, Vec2, Vec3};

use super::*;
use crate::clip::{AnimationClip, JointTrack, TrackCurve};
use crate::pose::translated;
use crate::skeleton::Skeleton;

const ROOT: u32 = 0;
const FOOT_L: u32 = 1;
const FOOT_R: u32 = 2;
const DT: f32 = 1.0 / 30.0;

/// A root with two feet hanging 0.1 m to each side.
fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "foot_l".into(), "foot_r".into()],
        vec![None, Some(ROOT), Some(ROOT)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(-0.1, 0.0, 0.0)),
            translated(Vec3::new(0.1, 0.0, 0.0)),
        ],
    )
    .expect("valid skeleton")
}

fn schema() -> FeatureSchema {
    FeatureSchema::new(ROOT, vec![FOOT_L, FOOT_R])
}

/// A locomotion clip keyed at 60 Hz: the root moves at `speed` m/s while turning at `turn`
/// rad/s (toward +X for positive `turn`), and the feet swing `stride` meters back and forth with
/// period `period`, lifting on their forward swing.
fn locomotion(name: &str, speed: f32, turn: f32, stride: f32, period: f32, duration: f32) -> AnimationClip {
    let keys = (duration * 60.0).round() as usize;
    let times: Vec<f32> = (0..=keys).map(|k| (k as f32 / 60.0).min(duration)).collect();
    let root_at = |t: f32| {
        if turn.abs() < 1e-6 {
            Vec3::new(0.0, 0.0, speed * t)
        } else {
            let r = speed / turn;
            Vec3::new(r * (1.0 - (turn * t).cos()), 0.0, r * (turn * t).sin())
        }
    };
    let phase = |t: f32| (TAU * t / period).sin();
    let mut clip = AnimationClip::empty(name, duration);
    clip.tracks.push(JointTrack {
        joint: ROOT,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| root_at(t)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Quat::from_rotation_y(turn * t)).collect(),
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    for (joint, side) in [(FOOT_L, 1.0f32), (FOOT_R, -1.0f32)] {
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times
                    .iter()
                    .map(|&t| {
                        let s = side * phase(t);
                        Vec3::new(-0.1 * side, 0.05 * s.max(0.0), stride * s)
                    })
                    .collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip
}

fn walk() -> AnimationClip {
    locomotion("walk", 1.5, 0.0, 0.3, 1.0, 1.0)
}

fn run() -> AnimationClip {
    locomotion("run", 4.0, 0.0, 0.6, 0.5, 1.0)
}

fn looping(clips: &[AnimationClip]) -> Vec<MotionClip<'_>> {
    clips
        .iter()
        .map(|clip| MotionClip { clip, looping: true })
        .collect()
}

fn build(clips: &[AnimationClip]) -> MotionDatabase {
    MotionDatabase::build(&skeleton(), &looping(clips), schema()).expect("database")
}

/// The trajectory of moving straight ahead at `speed` m/s.
fn straight(speed: f32) -> Vec<TrajectoryPoint> {
    DEFAULT_TRAJECTORY_TIMES
        .iter()
        .map(|&t| TrajectoryPoint::ahead(speed * t))
        .collect()
}

fn close(a: f32, b: f32, tolerance: f32, what: &str) {
    assert!((a - b).abs() <= tolerance, "{what}: {a} vs {b}");
}

fn close2(a: Vec2, b: Vec2, tolerance: f32, what: &str) {
    assert!(a.abs_diff_eq(b, tolerance), "{what}: {a} vs {b}");
}

fn close3(a: Vec3, b: Vec3, tolerance: f32, what: &str) {
    assert!(a.abs_diff_eq(b, tolerance), "{what}: {a} vs {b}");
}

// ---- Database layout ----------------------------------------------------------------------------

#[test]
fn schema_layout_is_contiguous_and_complete() {
    let schema = schema();
    assert_eq!(schema.dimensions(), 3 + 4 * 3 + 6 * 2);
    let mut next = 0;
    for group in FeatureGroup::ALL {
        let range = schema.group_range(group);
        assert_eq!(
            range.start,
            next,
            "{} starts where the previous ends",
            group.name()
        );
        next = range.end;
    }
    assert_eq!(next, schema.dimensions());
    assert_eq!(schema.group_range(FeatureGroup::TrajectoryPosition).len(), 6);
    assert_eq!(schema.group_range(FeatureGroup::FootVelocity).len(), 6);
    // No feet: the foot groups are empty and the vector is only root + trajectory.
    let bare = FeatureSchema::new(ROOT, Vec::new());
    assert_eq!(bare.dimensions(), 15);
    assert!(bare.group_range(FeatureGroup::FootPosition).is_empty());
}

#[test]
fn frame_counts_and_frame_lookup() {
    let walk = walk();
    let sources = [
        MotionClip {
            clip: &walk,
            looping: true,
        },
        MotionClip {
            clip: &walk,
            looping: false,
        },
    ];
    let db = MotionDatabase::build(&skeleton(), &sources, schema()).expect("database");
    // A looping 1 s clip at 30 Hz: 30 frames (its end is its start). One that does not loop:
    // 31 frames (its last frame is its end).
    assert_eq!(db.clips()[0].frames, 30);
    assert_eq!(db.clips()[1].frames, 31);
    assert_eq!(db.clips()[1].start, 30);
    assert_eq!(db.len(), 61);
    assert_eq!(db.frame(30), Some(FrameRef { clip: 1, time: 0.0 }));
    close(db.frame(60).expect("last").time, 1.0, 1e-6, "last frame time");
    assert_eq!(db.frame(61), None);
    assert_eq!(db.raw_features(61), None);
    assert_eq!(db.features(0).map(<[f32]>::len), Some(db.dimensions()));

    // frame_at rounds to the nearest frame; a looping clip wraps, the other clamps.
    assert_eq!(db.frame_at(0, 0.0), Some(0));
    assert_eq!(db.frame_at(0, 0.49 / 30.0), Some(0));
    assert_eq!(db.frame_at(0, 0.51 / 30.0), Some(1));
    assert_eq!(db.frame_at(0, 1.0), Some(0), "the loop end is frame 0");
    assert_eq!(db.frame_at(0, 1.0 + 2.0 / 30.0), Some(2));
    assert_eq!(db.frame_at(0, -1.0 / 30.0), Some(29));
    assert_eq!(db.frame_at(1, 5.0), Some(60));
    assert_eq!(db.frame_at(1, -5.0), Some(30));
    assert_eq!(db.frame_at(2, 0.0), None, "unknown clip");
    assert_eq!(db.frame_at(0, f32::NAN), None);
}

#[test]
fn exclusion_windows_wrap_in_looping_clips_and_clamp_otherwise() {
    let walk = walk();
    let sources = [
        MotionClip {
            clip: &walk,
            looping: true,
        },
        MotionClip {
            clip: &walk,
            looping: false,
        },
    ];
    let db = MotionDatabase::build(&skeleton(), &sources, schema()).expect("database");
    let skipped = |e: Exclusion| (0..db.len()).filter(|&f| e.contains(f)).collect::<Vec<_>>();

    assert_eq!(skipped(db.exclusion_around(10, 2)), vec![8, 9, 10, 11, 12]);
    // Frame 1 of the 30-frame loop: the window wraps to frames 28 and 29.
    assert_eq!(skipped(db.exclusion_around(1, 3)), vec![0, 1, 2, 3, 4, 28, 29]);
    assert_eq!(
        skipped(db.exclusion_around(28, 3)),
        vec![0, 1, 25, 26, 27, 28, 29]
    );
    assert_eq!(db.exclusion_around(1, 3).len(), 7);
    // A window wider than the loop covers exactly the clip.
    assert_eq!(skipped(db.exclusion_around(5, 40)), (0..30).collect::<Vec<_>>());
    // The clip that does not loop clamps at both ends and never reaches the other clip.
    assert_eq!(skipped(db.exclusion_around(31, 3)), vec![30, 31, 32, 33, 34]);
    assert_eq!(skipped(db.exclusion_around(60, 2)), vec![58, 59, 60]);
    // Radius 0 skips only the frame itself; an unknown frame skips nothing.
    assert_eq!(skipped(db.exclusion_around(7, 0)), vec![7]);
    assert!(db.exclusion_around(999, 3).is_empty());
    assert!(Exclusion::NONE.is_empty());
    assert_eq!(Exclusion::range(3..9).len(), 6);
}

/// Audit (corr-animation): `exclude_window` accepts any finite value, and `1e30 * rate` saturates
/// to `usize::MAX`; `2 * radius + 1` then overflowed (a panic with overflow checks, a wrapped
/// window that missed the playing frame without them).
#[test]
fn a_huge_exclusion_radius_covers_the_clip_without_overflow() {
    let walk = walk();
    let sources = [
        MotionClip {
            clip: &walk,
            looping: true,
        },
        MotionClip {
            clip: &walk,
            looping: false,
        },
    ];
    let db = MotionDatabase::build(&skeleton(), &sources, schema()).expect("database");
    let skipped = |e: Exclusion| (0..db.len()).filter(|&f| e.contains(f)).collect::<Vec<_>>();
    assert_eq!(
        skipped(db.exclusion_around(5, usize::MAX)),
        (0..30).collect::<Vec<_>>()
    );
    assert_eq!(
        skipped(db.exclusion_around(45, usize::MAX)),
        (30..61).collect::<Vec<_>>()
    );
    let radius = (1.0e30f32 * db.schema().sample_rate).round() as usize;
    assert_eq!(
        skipped(db.exclusion_around(45, radius)),
        (30..61).collect::<Vec<_>>()
    );
}

/// Audit (corr-animation): a cooked database never ran `FeatureSchema::validate`, so a corrupt
/// sample rate of 0 or NaN loaded and fed `2.0 / sample_rate` and the exclusion radius.
#[test]
fn a_cooked_database_with_a_bad_sample_rate_is_rejected() {
    for rate in [0.0, -30.0, f32::NAN, f32::INFINITY] {
        let mut db = build(&[walk()]);
        db.schema.sample_rate = rate;
        let cooked = db.to_cooked_payload().expect("cook");
        assert!(
            MotionDatabase::from_cooked_payload(&cooked).is_err(),
            "rate {rate}"
        );
    }
}

// ---- Features -----------------------------------------------------------------------------------

#[test]
fn straight_walk_features_match_the_motion_including_across_the_loop() {
    let db = build(&[walk()]);
    for frame in [0usize, 7, 25, 29] {
        let q = db.frame_query(frame).expect("frame");
        close3(
            q.root_velocity,
            Vec3::new(0.0, 0.0, 1.5),
            1e-3,
            &format!("root velocity, frame {frame}"),
        );
        for (point, &ahead) in q.trajectory.iter().zip(&DEFAULT_TRAJECTORY_TIMES) {
            // Frames 25 and 29 look past the end of the clip: the next lap continues the line.
            close2(
                point.position,
                Vec2::new(0.0, 1.5 * ahead),
                1e-3,
                &format!("position, frame {frame}"),
            );
            close2(
                point.direction,
                Vec2::Y,
                1e-5,
                &format!("direction, frame {frame}"),
            );
        }
    }
    // Frame 0: the left foot is at the swing's center, 0.1 m to the left (-X), moving forward at
    // the root speed plus the swing speed (2π · 0.3 m/s).
    let q = db.frame_query(0).expect("frame 0");
    close3(
        q.feet[0].position,
        Vec3::new(-0.1, 0.0, 0.0),
        1e-4,
        "left foot position",
    );
    close3(
        q.feet[1].position,
        Vec3::new(0.1, 0.0, 0.0),
        1e-4,
        "right foot position",
    );
    // A central difference over ±1/30 s of a sine: 2π·0.3·sinc-like factor; within 2 %.
    let swing = TAU * 0.3;
    close(
        q.feet[0].velocity.z,
        1.5 + swing,
        0.02 * swing,
        "left foot forward speed",
    );
    close(
        q.feet[1].velocity.z,
        1.5 - swing,
        0.02 * swing,
        "right foot forward speed",
    );
}

#[test]
fn turning_features_rotate_into_the_character_frame() {
    // 2 m/s while turning 1 rad/s toward +X: a circle of radius 2.
    let db = build(&[locomotion("turn", 2.0, 1.0, 0.3, 1.0, 1.0)]);
    for frame in [0usize, 12, 27] {
        let q = db.frame_query(frame).expect("frame");
        // Velocity is always straight ahead in the character's own frame.
        close3(
            q.root_velocity,
            Vec3::new(0.0, 0.0, 2.0),
            0.01,
            &format!("velocity, frame {frame}"),
        );
        for (point, &ahead) in q.trajectory.iter().zip(&DEFAULT_TRAJECTORY_TIMES) {
            let expected = Vec2::new(2.0 * (1.0 - ahead.cos()), 2.0 * ahead.sin());
            close2(
                point.position,
                expected,
                2e-3,
                &format!("position {ahead}, frame {frame}"),
            );
            close2(
                point.direction,
                Vec2::new(ahead.sin(), ahead.cos()),
                1e-3,
                "direction",
            );
        }
    }
}

#[test]
fn clip_that_does_not_loop_clamps_its_future_to_the_last_frame() {
    let walk = walk();
    let db = MotionDatabase::build(
        &skeleton(),
        &[MotionClip {
            clip: &walk,
            looping: false,
        }],
        schema(),
    )
    .expect("database");
    let last = db.len() - 1;
    let q = db.frame_query(last).expect("last frame");
    for point in &q.trajectory {
        close2(point.position, Vec2::ZERO, 1e-5, "no future past the end");
    }
    // Two thirds of a second before the end: the first point moves, the last two clamp.
    let q = db.frame_query(last - 20).expect("frame");
    close(q.trajectory[0].position.y, 0.5, 1e-3, "1/3 s ahead");
    close(
        q.trajectory[1].position.y,
        1.0,
        1e-3,
        "2/3 s ahead (exactly the end)",
    );
    close(q.trajectory[2].position.y, 1.0, 1e-3, "1 s ahead clamps");
    // One-sided velocity at the ends is still the walk speed (the motion is linear).
    close3(q.root_velocity, Vec3::new(0.0, 0.0, 1.5), 1e-3, "velocity");
    close3(
        db.frame_query(0).expect("first").root_velocity,
        Vec3::new(0.0, 0.0, 1.5),
        1e-3,
        "first",
    );
}

#[test]
fn custom_forward_axis_defines_the_facing() {
    // The same straight walk with the root's +X declared as forward: the character "faces"
    // 90 degrees to the right of its motion, so the motion is along its local -X.
    let mut schema = schema();
    schema.forward = Vec3::X;
    let db = MotionDatabase::build(&skeleton(), &looping(&[walk()]), schema).expect("database");
    let q = db.frame_query(3).expect("frame");
    close3(
        q.root_velocity,
        Vec3::new(-1.5, 0.0, 0.0),
        1e-3,
        "velocity in the X-forward frame",
    );
    close2(q.trajectory[2].position, Vec2::new(-1.5, 0.0), 1e-3, "trajectory");
    close2(
        q.trajectory[2].direction,
        Vec2::Y,
        1e-5,
        "still facing its own forward",
    );
}

// ---- Normalization ------------------------------------------------------------------------------

#[test]
fn normalization_centers_each_group_and_scales_it_to_its_weight() {
    let mut schema = schema();
    schema.weights.trajectory_position = 2.0;
    schema.weights.foot_velocity = 0.0;
    let clips = [walk(), run(), locomotion("turn", 2.0, 1.0, 0.4, 1.0, 1.0)];
    let db = MotionDatabase::build(&skeleton(), &looping(&clips), schema.clone()).expect("database");
    let n = db.len() as f64;
    // Every group the schema lays out (the history and pose groups are empty here).
    for group in FeatureGroup::ALL
        .into_iter()
        .filter(|g| !schema.group_range(*g).is_empty())
    {
        let range = schema.group_range(group);
        let mut sum_sq = 0.0f64;
        for frame in 0..db.len() {
            let f = db.features(frame).expect("frame");
            sum_sq += f[range.clone()]
                .iter()
                .map(|&v| f64::from(v).powi(2))
                .sum::<f64>();
        }
        for dim in range.clone() {
            let mean = (0..db.len())
                .map(|frame| f64::from(db.features(frame).expect("frame")[dim]))
                .sum::<f64>()
                / n;
            assert!(mean.abs() < 1e-4, "{} dim {dim} mean {mean}", group.name());
        }
        // The group's root-mean-square over its dimensions is its weight.
        let rms = (sum_sq / (n * range.len() as f64)).sqrt();
        let weight = f64::from(schema.weights.get(group));
        assert!(
            (rms - weight).abs() < 1e-3,
            "{} rms {rms} weight {weight}",
            group.name()
        );
    }
    // Weight 0: every foot velocity normalizes to zero, so it never affects the search.
    let velocities = schema.group_range(FeatureGroup::FootVelocity);
    assert!(db.scale()[velocities].iter().all(|&s| s == 0.0));
}

#[test]
fn constant_group_divides_by_one_and_normalizes_to_zero() {
    // Every clip walks straight: trajectory directions are constant (0, 1) in every frame.
    let db = build(&[walk(), run()]);
    let directions = db.schema().group_range(FeatureGroup::TrajectoryDirection);
    assert!(db.scale()[directions.clone()].iter().all(|&s| s == 1.0));
    for frame in 0..db.len() {
        let f = db.features(frame).expect("frame");
        assert!(f[directions.clone()].iter().all(|v| v.abs() < 1e-6));
    }
    // A query that turns around costs exactly its direction error (scale 1).
    let mut query = db.frame_query(0).expect("frame");
    let base = db.normalize(&query).expect("query");
    for point in &mut query.trajectory {
        point.direction = Vec2::NEG_Y;
    }
    let turned = db.normalize(&query).expect("query");
    let extra = db.cost(&turned, 0).expect("cost") - db.cost(&base, 0).expect("cost");
    close(extra, 3.0 * 4.0, 1e-3, "three points, each (0,1)→(0,-1): 2² each");
}

// ---- Search -------------------------------------------------------------------------------------

#[test]
fn single_clip_database_returns_that_clip() {
    for looping_clip in [true, false] {
        let walk = walk();
        let db = MotionDatabase::build(
            &skeleton(),
            &[MotionClip {
                clip: &walk,
                looping: looping_clip,
            }],
            schema(),
        )
        .expect("database");
        assert_eq!(db.clips().len(), 1);
        assert_eq!(db.clips()[0].name, "walk");
        // Whatever the query asks (stand, walk, run, turn back), the answer is in the walk.
        for trajectory in [straight(0.0), straight(1.5), straight(6.0), straight(-2.0)] {
            let query = db.query_with_mean_pose(&trajectory);
            for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
                let hit = db
                    .search_query(&query, Exclusion::NONE, method)
                    .expect("query")
                    .expect("a frame");
                assert!(db.clips()[0].range().contains(&hit.frame));
                assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(0));
            }
        }
        // A frame's own features find that frame at zero cost.
        for frame in 0..db.len() {
            let query = db.frame_query(frame).expect("frame");
            let hit = db
                .search_query(&query, Exclusion::NONE, SearchMethod::KdTree)
                .expect("query")
                .expect("a frame");
            assert_eq!(hit.frame, frame);
            assert!(hit.cost < 1e-6, "frame {frame} cost {}", hit.cost);
        }
        // The matcher starts on the clip and stays in it.
        let mut matcher = MotionMatcher::new(MatcherSettings::default());
        for _ in 0..90 {
            let step = matcher.update(&db, &straight(1.5), DT).expect("update");
            assert_eq!(step.playback.clip, 0);
        }
        // Every frame excluded: no answer, not a panic.
        let all = Exclusion::range(0..db.len());
        let query = db.normalize(&db.frame_query(0).expect("frame")).expect("query");
        assert_eq!(db.search(&query, all, SearchMethod::KdTree), None);
        assert_eq!(db.search(&query, all, SearchMethod::BruteForce), None);
    }
}

#[test]
fn straight_walk_query_picks_the_walk_over_the_run() {
    let db = build(&[walk(), run()]);
    let walk_clip = 0;
    let run_clip = 1;
    for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
        let mut walk_query = db.query_with_mean_pose(&straight(1.5));
        walk_query.root_velocity = Vec3::new(0.0, 0.0, 1.5);
        let hit = db
            .search_query(&walk_query, Exclusion::NONE, method)
            .expect("query")
            .expect("a frame");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(walk_clip), "{method:?}");

        let mut run_query = db.query_with_mean_pose(&straight(4.0));
        run_query.root_velocity = Vec3::new(0.0, 0.0, 4.0);
        let hit = db
            .search_query(&run_query, Exclusion::NONE, method)
            .expect("query")
            .expect("a frame");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(run_clip), "{method:?}");
    }

    // Every walk frame's pose with a walking path matches the walk; with a running path, the run.
    for frame in db.clips()[walk_clip].range() {
        let query = db.query_from_frame(frame, &straight(1.5)).expect("query");
        let hit = db
            .search_query(&query, Exclusion::NONE, SearchMethod::KdTree)
            .expect("query")
            .expect("a frame");
        assert_eq!(
            db.frame(hit.frame).map(|f| f.clip),
            Some(walk_clip),
            "frame {frame}"
        );
    }

    // At runtime: start walking, then ask for a run; the matcher moves to the run and stays.
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    for _ in 0..30 {
        matcher.update(&db, &straight(1.5), DT).expect("update");
    }
    assert_eq!(matcher.playback().map(|p| p.clip), Some(walk_clip));
    let jumps_walking = matcher.jumps();
    let mut first_run = None;
    for step in 0..60 {
        let result = matcher.update(&db, &straight(4.0), DT).expect("update");
        if result.playback.clip == run_clip && first_run.is_none() {
            first_run = Some(step);
        }
    }
    let first_run = first_run.expect("the matcher must switch to the run");
    assert!(
        first_run <= 3,
        "the next search (0.1 s) switches, got step {first_run}"
    );
    assert_eq!(matcher.playback().map(|p| p.clip), Some(run_clip));
    assert!(matcher.jumps() > jumps_walking);
}

/// `last_query()` is documented as the *latest search's* query. A failed search (a non-finite
/// trajectory point) used to overwrite it with the raw, unnormalised — possibly NaN —
/// in-progress vector, which the candidate report then ranked against.
#[test]
fn a_failed_search_leaves_last_query_unchanged() {
    let db = build(&[walk()]);
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    matcher
        .update(&db, &straight(1.5), DT)
        .expect("the first pick searches");
    let good = matcher.last_query().to_vec();
    assert!(!good.is_empty());
    assert!(good.iter().all(|v| v.is_finite()));

    matcher.request_search();
    let mut bad = straight(1.5);
    bad[0].position = Vec2::new(f32::NAN, 0.0);
    let err = matcher
        .update(&db, &bad, DT)
        .expect_err("a NaN trajectory point is refused");
    assert!(matches!(err, MotionMatchError::NonFiniteQuery));
    assert_eq!(
        matcher.last_query(),
        good.as_slice(),
        "the failed search must not clobber the last successful query"
    );
}

/// Two walks that differ only by 4 cm/s, and a desired path that alternates between slightly
/// slower than one and slightly faster than the other at every update.
fn flicker_run(settings: MatcherSettings, updates: usize) -> (MotionMatcher, Vec<MatchStep>) {
    let db = build(&[
        locomotion("walk_a", 1.50, 0.0, 0.3, 1.0, 1.0),
        locomotion("walk_b", 1.54, 0.0, 0.3, 1.0, 1.0),
    ]);
    let mut matcher = MotionMatcher::new(settings);
    let steps = (0..updates)
        .map(|i| {
            let speed = if i.is_multiple_of(2) { 1.58 } else { 1.46 };
            matcher.update(&db, &straight(speed), DT).expect("update")
        })
        .collect();
    (matcher, steps)
}

#[test]
fn transition_cost_prevents_flicker() {
    let eager = MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        transition_cost: 0.0,
        ..MatcherSettings::default()
    };
    let (flicker, steps) = flicker_run(eager, 60);
    // Without a transition cost every search prefers the other walk: the clip flips at almost
    // every update.
    let flips = steps
        .windows(2)
        .filter(|w| w[0].playback.clip != w[1].playback.clip)
        .count();
    assert!(
        flips >= 50,
        "expected flicker, got {flips} clip changes in 60 updates"
    );
    assert!(flicker.jumps() >= 50);

    // The same inputs with a transition cost larger than the alternating advantage: one pick,
    // then the continuation always wins.
    let damped = MatcherSettings {
        transition_cost: 200.0,
        ..eager
    };
    let (steady, steps) = flicker_run(damped, 60);
    assert_eq!(steady.jumps(), 1, "only the first pick");
    // The first query asks for 1.58 m/s, so the first pick is walk_b; the last (update 59)
    // asks for 1.46 m/s, which walk_a matches better.
    assert_eq!(steps[0].playback.clip, 1);
    let clip = steps[0].playback.clip;
    assert!(steps.iter().all(|s| s.playback.clip == clip));
    assert!(steps.iter().skip(1).all(|s| !s.jumped && s.searched));
    let report = steady.last_search().expect("a search ran");
    assert!(!report.jumped && !report.held);
    let best = report.best.expect("a candidate").cost;
    let stay = report.continuation_cost.expect("a continuation");
    assert!(
        best < stay,
        "the other walk is cheaper before the transition cost"
    );
    assert!(best + 200.0 >= stay, "but not by more than the transition cost");
}

#[test]
fn minimum_time_between_jumps_holds_back_jumps() {
    let eager = MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        transition_cost: 0.0,
        ..MatcherSettings::default()
    };
    let (free, _) = flicker_run(eager, 90);
    let limited = MatcherSettings {
        min_jump_interval: 0.5,
        ..eager
    };
    let (held, steps) = flicker_run(limited, 90);
    // 90 updates at 1/30 s = 3 s: the first pick plus at most one jump per 0.5 s.
    assert!(held.jumps() <= 1 + 6, "{} jumps", held.jumps());
    assert!(
        held.jumps() >= 2,
        "jumps still happen once the interval has passed"
    );
    assert!(free.jumps() > 3 * held.jumps());
    // Consecutive jumps are at least 0.5 s apart (15 updates at 1/30 s).
    let jump_steps: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.jumped)
        .map(|(i, _)| i)
        .collect();
    for pair in jump_steps.windows(2) {
        assert!(pair[1] - pair[0] >= 15, "jumps at {pair:?}");
    }
}

#[test]
fn search_interval_spaces_the_searches() {
    let db = build(&[walk(), run()]);
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.1,
        ..MatcherSettings::default()
    });
    let steps: Vec<MatchStep> = (0..30)
        .map(|_| matcher.update(&db, &straight(1.5), DT).expect("update"))
        .collect();
    // The first update searches (nothing plays), then every third update (0.1 s at 1/30 s).
    assert!(steps[0].searched && steps[0].jumped);
    let searched: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.searched)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(searched, vec![0, 3, 6, 9, 12, 15, 18, 21, 24, 27]);
    assert_eq!(matcher.searches(), 10);
    // Between searches the playing clip advances by dt.
    let a = steps[1].playback;
    let b = steps[2].playback;
    assert_eq!(a.clip, b.clip);
    // Both clips last 1 s and loop.
    close((b.time - a.time).rem_euclid(1.0), DT, 1e-5, "advance");
}

#[test]
fn ended_clip_that_does_not_loop_searches_again() {
    // A 0.5 s clip that does not loop is the only clip, and the exclusion window covers all of
    // it. Searches are 100 s apart and jumps cost a million: only reaching the end can search,
    // and then any frame but the end one is taken.
    let stop = locomotion("stop", 1.5, 0.0, 0.3, 1.0, 0.5);
    let db = MotionDatabase::build(
        &skeleton(),
        &[MotionClip {
            clip: &stop,
            looping: false,
        }],
        schema(),
    )
    .expect("database");
    let end = db.len() - 1;
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 100.0,
        min_jump_interval: 100.0,
        transition_cost: 1.0e6,
        exclude_window: 10.0,
        ..MatcherSettings::default()
    });
    let first = matcher.update(&db, &straight(1.5), DT).expect("update");
    assert!(first.searched && first.jumped);
    let mut forced = Vec::new();
    for step in 1..60 {
        let result = matcher.update(&db, &straight(1.5), DT).expect("update");
        if result.searched {
            forced.push(result);
            // The search ran because the clip had reached its end.
            assert!(result.jumped, "step {step}: nothing can continue an ended clip");
            assert_ne!(result.playback.frame, end, "never restart on the end frame");
            let report = matcher.last_search().expect("report");
            assert_eq!(report.continuation_cost, None);
        }
    }
    // 59 updates of 1/30 s through a clip of at most 0.5 s: it ends at least three times.
    assert!(forced.len() >= 3, "{} forced searches", forced.len());
    assert_eq!(matcher.jumps(), 1 + forced.len() as u64);
}

#[test]
fn identical_frames_tie_to_the_lowest_index_with_both_methods() {
    let db = build(&[walk(), walk()]);
    for frame in 30..60 {
        let query = db
            .normalize(&db.frame_query(frame).expect("frame"))
            .expect("query");
        let brute = db.search(&query, Exclusion::NONE, SearchMethod::BruteForce);
        let kd = db.search(&query, Exclusion::NONE, SearchMethod::KdTree);
        assert_eq!(brute, kd);
        assert_eq!(brute.map(|h| h.frame), Some(frame - 30), "the first copy wins");
    }
}

// ---- k-d tree -----------------------------------------------------------------------------------

fn random_rows(rng: &mut Rng, rows: usize, dims: usize, quantize: bool) -> Vec<f32> {
    (0..rows * dims)
        .map(|_| {
            let v = rng.f32() * 2.0 - 1.0;
            // Quantized data has many exact ties and duplicate rows.
            if quantize { (v * 3.0).round() / 3.0 } else { v }
        })
        .collect()
}

#[test]
fn kd_tree_equals_brute_force_on_random_data() {
    let mut rng = Rng::new(0x6d6f_7469_6f6e);
    for &(rows, dims) in &[
        (1usize, 1usize),
        (7, 2),
        (8, 3),
        (9, 3),
        (100, 1),
        (257, 4),
        (1000, 27),
        (2000, 8),
    ] {
        for quantize in [false, true] {
            let data = random_rows(&mut rng, rows, dims, quantize);
            let tree = KdTree::build(&data, dims).expect("tree");
            assert_eq!(tree.len(), rows);
            assert_eq!(tree.dims(), dims);
            for q in 0..200usize {
                let query = random_rows(&mut rng, 1, dims, quantize && q.is_multiple_of(3));
                let skip_mod = 2 + (q % 5);
                let exclude = |row: usize| q.is_multiple_of(4) && row.is_multiple_of(skip_mod);
                let brute = nearest_brute_force(&data, dims, &query, exclude);
                let kd = tree.nearest(&query, exclude);
                assert_eq!(kd, brute, "rows {rows} dims {dims} quantize {quantize} query {q}");
                assert_eq!(kd.is_some(), (0..rows).any(|row| !exclude(row)));
            }
        }
    }
}

#[test]
fn kd_tree_shape_and_edge_cases() {
    assert!(KdTree::build(&[1.0, 2.0, 3.0], 0).is_none(), "zero dimensions");
    assert!(KdTree::build(&[1.0, 2.0, 3.0], 2).is_none(), "ragged rows");
    let empty = KdTree::build(&[], 3).expect("empty tree");
    assert!(empty.is_empty());
    assert_eq!(empty.nearest(&[0.0, 0.0, 0.0], |_| false), None);
    assert_eq!(empty.stats(), KdStats::default());

    let mut rng = Rng::new(7);
    let data = random_rows(&mut rng, 1024, 4, false);
    let tree = KdTree::build(&data, 4).expect("tree");
    let stats = tree.stats();
    // Median splits down to leaves of at most 8 rows: 1024 rows → 128 leaves, depth 8.
    assert_eq!(stats.leaves, 128);
    assert_eq!(stats.nodes, 255);
    assert_eq!(stats.depth, 8);
    assert_eq!(tree.nearest(&[0.0; 3], |_| false), None, "wrong query length");
    assert_eq!(tree.nearest(&[0.0; 4], |_| true), None, "everything excluded");

    // All rows identical: one leaf, and the lowest index wins.
    let same = vec![0.5f32; 40 * 2];
    let tree = KdTree::build(&same, 2).expect("tree");
    assert_eq!(tree.stats().nodes, 1);
    let hit = tree.nearest(&[0.0, 0.0], |row| row < 3).expect("hit");
    assert_eq!(hit.row, 3);
    assert_eq!(hit.distance, 0.5);
}

#[test]
fn kd_search_equals_brute_force_on_a_random_clip_database() {
    let mut rng = Rng::new(0x2016_0316);
    let clips: Vec<AnimationClip> = (0..6)
        .map(|i| {
            locomotion(
                &format!("clip{i}"),
                rng.range(0.0f32..5.0),
                rng.range(-1.5f32..1.5),
                rng.range(0.1f32..0.7),
                [0.5f32, 1.0][i % 2],
                1.0,
            )
        })
        .collect();
    let db = build(&clips);
    assert_eq!(db.len(), 180);
    for q in 0..300usize {
        let frame = (rng.next_u32() as usize) % db.len();
        let mut query = db.frame_query(frame).expect("frame");
        query.root_velocity += Vec3::new(rng.f32() - 0.5, 0.0, rng.f32() * 4.0 - 2.0);
        for point in &mut query.trajectory {
            point.position += Vec2::new(rng.f32() - 0.5, rng.f32() * 2.0 - 1.0);
        }
        for foot in &mut query.feet {
            foot.velocity += Vec3::new(0.0, 0.0, rng.f32() - 0.5);
        }
        let normalized = db.normalize(&query).expect("query");
        let exclude = if q.is_multiple_of(2) {
            db.exclusion_around(frame, 7)
        } else {
            Exclusion::NONE
        };
        let brute = db.search(&normalized, exclude, SearchMethod::BruteForce);
        let kd = db.search(&normalized, exclude, SearchMethod::KdTree);
        assert_eq!(kd, brute, "query {q}");
        let hit = kd.expect("a frame");
        assert_eq!(db.cost(&normalized, hit.frame), Some(hit.cost));
        assert!(!exclude.contains(hit.frame));
    }
}

// ---- Determinism --------------------------------------------------------------------------------

#[test]
fn building_and_matching_are_deterministic() {
    let clips = [walk(), run(), locomotion("turn", 2.0, 1.0, 0.4, 1.0, 1.0)];
    let a = build(&clips);
    let b = build(&clips);
    assert_eq!(a.len(), b.len());
    for frame in 0..a.len() {
        assert_eq!(a.raw_features(frame), b.raw_features(frame));
        assert_eq!(a.features(frame), b.features(frame));
        assert_eq!(a.frame(frame), b.frame(frame));
    }
    assert_eq!(a.mean(), b.mean());
    assert_eq!(a.scale(), b.scale());
    assert_eq!(a.kd_stats(), b.kd_stats());

    // The same inputs give the same steps, and the k-d tree steps exactly like the scan.
    let script = |db: &MotionDatabase, method: SearchMethod| {
        let mut matcher = MotionMatcher::new(MatcherSettings {
            method,
            ..MatcherSettings::default()
        });
        (0..240)
            .map(|i| {
                let speed = [0.0, 1.5, 4.0, 2.0][(i / 60) % 4];
                matcher.update(db, &straight(speed), DT).expect("update")
            })
            .collect::<Vec<_>>()
    };
    let first = script(&a, SearchMethod::KdTree);
    assert_eq!(first, script(&b, SearchMethod::KdTree));
    assert_eq!(first, script(&a, SearchMethod::BruteForce));
    assert!(
        first.iter().filter(|s| s.jumped).count() >= 2,
        "the script changes clips"
    );
}

// ---- Errors -------------------------------------------------------------------------------------

#[test]
fn build_rejects_bad_input() {
    let skeleton = skeleton();
    let walk = walk();
    let ok = [MotionClip {
        clip: &walk,
        looping: true,
    }];
    let err = |schema: FeatureSchema| MotionDatabase::build(&skeleton, &ok, schema).err();

    assert_eq!(
        MotionDatabase::build(&skeleton, &[], schema()).err(),
        Some(MotionMatchError::NoClips)
    );
    for rate in [0.0, -30.0, f32::NAN, f32::INFINITY] {
        let mut s = schema();
        s.sample_rate = rate;
        assert!(
            matches!(err(s), Some(MotionMatchError::BadSampleRate(_))),
            "rate {rate}"
        );
    }
    for time in [0.0, -0.5, f32::NAN] {
        let mut s = schema();
        s.trajectory_times.push(time);
        assert!(
            matches!(err(s), Some(MotionMatchError::BadTrajectoryTime(_))),
            "time {time}"
        );
    }
    let mut s = schema();
    s.weights.foot_position = -1.0;
    assert_eq!(
        err(s),
        Some(MotionMatchError::BadWeight {
            group: "foot_position",
            value: -1.0
        })
    );
    let mut s = schema();
    s.weights.root_velocity = f32::INFINITY;
    assert!(matches!(
        err(s),
        Some(MotionMatchError::BadWeight {
            group: "root_velocity",
            ..
        })
    ));
    for forward in [Vec3::Y, Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 1.0)] {
        let mut s = schema();
        s.forward = forward;
        assert!(
            matches!(err(s), Some(MotionMatchError::BadForward(_))),
            "forward {forward}"
        );
    }
    let mut s = schema();
    s.feet.push(9);
    assert_eq!(
        err(s),
        Some(MotionMatchError::JointOutOfRange { joint: 9, joints: 3 })
    );
    let mut s = schema();
    s.root_joint = 3;
    assert_eq!(
        err(s),
        Some(MotionMatchError::JointOutOfRange { joint: 3, joints: 3 })
    );

    for duration in [0.0, -1.0, f32::NAN] {
        let mut bad = walk.clone();
        bad.duration = duration;
        let sources = [MotionClip {
            clip: &bad,
            looping: false,
        }];
        assert!(matches!(
            MotionDatabase::build(&skeleton, &sources, schema()),
            Err(MotionMatchError::BadClipDuration { .. })
        ));
    }

    let mut poisoned = walk.clone();
    if let TrackCurve::Animated { values, .. } = &mut poisoned.tracks[1].translations {
        values[12] = Vec3::new(f32::NAN, 0.0, 0.0);
    }
    let sources = [MotionClip {
        clip: &poisoned,
        looping: true,
    }];
    assert!(matches!(
        MotionDatabase::build(&skeleton, &sources, schema()),
        Err(MotionMatchError::NonFiniteFeature { ref clip, .. }) if clip == "walk"
    ));
}

#[test]
fn queries_of_the_wrong_shape_or_not_finite_are_errors() {
    let db = build(&[walk(), run()]);
    let short = db.query_with_mean_pose(&straight(1.0)[..2]);
    assert_eq!(
        db.normalize(&short),
        Err(MotionMatchError::QueryShape {
            part: "trajectory points",
            got: 2,
            expected: 3
        })
    );
    let mut one_foot = db.frame_query(0).expect("frame");
    one_foot.feet.pop();
    assert_eq!(
        db.normalize(&one_foot),
        Err(MotionMatchError::QueryShape {
            part: "feet",
            got: 1,
            expected: 2
        })
    );
    let mut nan = db.frame_query(0).expect("frame");
    nan.trajectory[1].position.x = f32::NAN;
    assert_eq!(db.normalize(&nan), Err(MotionMatchError::NonFiniteQuery));
    assert_eq!(
        db.query_from_frame(999, &straight(1.0)),
        Err(MotionMatchError::FrameOutOfRange {
            frame: 999,
            frames: 60
        })
    );
    // A normalized query of the wrong length has no cost and no answer.
    assert_eq!(db.cost(&[0.0; 4], 0), None);
    assert_eq!(db.search(&[0.0; 4], Exclusion::NONE, SearchMethod::KdTree), None);
    assert_eq!(
        db.search(&[0.0; 4], Exclusion::NONE, SearchMethod::BruteForce),
        None
    );

    // The matcher reports the error and does not jump.
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    assert!(matcher.update(&db, &straight(1.0)[..1], DT).is_err());
    assert_eq!(matcher.playback(), None);
    assert_eq!(matcher.jumps(), 0);
    let step = matcher.update(&db, &straight(1.0), DT).expect("update");
    assert!(step.jumped);
    // Bad settings and a bad dt are sanitized, not propagated.
    matcher.settings.search_interval = f32::NAN;
    matcher.settings.transition_cost = -5.0;
    for dt in [f32::NAN, -1.0, f32::INFINITY] {
        let step = matcher.update(&db, &straight(1.0), dt).expect("update");
        assert!(step.playback.time.is_finite());
    }
    // Reset forgets what plays but keeps the counters (spec 52 § 2.8 state table).
    let searches = matcher.searches();
    matcher.reset();
    assert_eq!(matcher.playback(), None);
    assert_eq!(matcher.searches(), searches);

    // Playing the run (clip 1), then handed a database with one clip: it starts over there
    // instead of indexing a clip that is not there.
    for _ in 0..10 {
        matcher.update(&db, &straight(4.0), DT).expect("update");
    }
    assert_eq!(matcher.playback().map(|p| p.clip), Some(1));
    let walk_only = build(&[walk()]);
    let step = matcher.update(&walk_only, &straight(4.0), DT).expect("update");
    assert!(step.jumped && step.searched);
    assert_eq!(step.playback.clip, 0);
}

// ---- ECS ----------------------------------------------------------------------------------------

#[test]
fn component_steps_through_the_system() {
    use regia_app::prelude::*;

    let db = Arc::new(build(&[walk(), run()]));
    let mut app = App::new();
    app.add_plugins(regia_app::MinimalPlugins);
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(20),
    ));
    app.add_systems(Update, step_motion_matching);
    let mut matching = MotionMatching::new(db.clone(), MatcherSettings::default());
    assert_eq!(matching.trajectory.len(), 3);
    matching.trajectory = straight(4.0);
    let runner = app.world_mut().spawn(matching).id();
    let broken = app
        .world_mut()
        .spawn(MotionMatching {
            trajectory: Vec::new(),
            ..MotionMatching::new(db, MatcherSettings::default())
        })
        .id();
    for _ in 0..30 {
        app.update();
    }
    let matching = app.world().get::<MotionMatching>(runner).expect("component");
    let step = matching.last_step.expect("stepped");
    assert_eq!(step.playback.clip, 1, "a running path plays the run");
    assert!(matching.matcher.searches() >= 2);
    assert_eq!(matching.last_error, None);
    let broken = app.world().get::<MotionMatching>(broken).expect("component");
    assert_eq!(broken.last_step, None);
    assert!(matches!(
        broken.last_error,
        Some(MotionMatchError::QueryShape { got: 0, .. })
    ));
}

// ---- Hand-run timing (docs/06-testing-and-ci.md § 9) --------------------------------------------

/// p50, p99 and max of per-operation times, microseconds.
fn percentiles(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.99), at(1.0))
}

#[test]
#[ignore = "hand-run timing; run in release with --ignored --nocapture"]
fn perf_micro_motion_matching_search() {
    use std::time::Instant;

    // 20 clips of 20 s at 30 Hz: 12,000 frames of 27 floats.
    let mut rng = Rng::new(0x4d4d);
    let clips: Vec<AnimationClip> = (0..20)
        .map(|i| {
            locomotion(
                &format!("clip{i}"),
                rng.range(0.0f32..5.0),
                rng.range(-1.0f32..1.0),
                rng.range(0.1f32..0.7),
                [0.5f32, 1.0][i % 2],
                20.0,
            )
        })
        .collect();
    let started = Instant::now();
    let db = build(&clips);
    let build_ms = started.elapsed().as_secs_f64() * 1e3;
    assert_eq!(db.len(), 12_000);
    println!(
        "perf-micro motion_matching_build: n=1 frames={} ms={build_ms:.1} kd={:?}",
        db.len(),
        db.kd_stats()
    );

    let queries: Vec<Vec<f32>> = (0..2000)
        .map(|_| {
            let frame = (rng.next_u32() as usize) % db.len();
            let mut query = db.frame_query(frame).expect("frame");
            for point in &mut query.trajectory {
                point.position += Vec2::new(rng.f32() - 0.5, rng.f32() * 2.0 - 1.0);
            }
            db.normalize(&query).expect("query")
        })
        .collect();
    let mut found = 0usize;
    for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
        let times: Vec<f64> = queries
            .iter()
            .map(|q| {
                let started = Instant::now();
                let hit = db.search(q, Exclusion::NONE, method);
                let us = started.elapsed().as_secs_f64() * 1e6;
                found += usize::from(hit.is_some());
                us
            })
            .collect();
        let (p50, p99, max) = percentiles(times);
        println!(
            "perf-micro motion_matching_search_{method:?}: n={} p50={p50:.1}us p99={p99:.1}us max={max:.1}us",
            queries.len()
        );
    }
    assert_eq!(found, 2 * queries.len());
}

// ---- tags, spans, filters, biases (spec 52 § 2.5, § 2.8) ----

fn tag_table() -> TagTable {
    TagTable::new(vec!["walk".into(), "run".into()]).expect("table")
}

/// walk tagged `walk` (bit 0), run tagged `run` (bit 1).
fn tagged_db() -> MotionDatabase {
    let mut db = build(&[walk(), run()]);
    db.annotate(
        tag_table(),
        &[
            ClipAnnotation {
                tags: 0b01,
                spans: Vec::new(),
            },
            ClipAnnotation {
                tags: 0b10,
                spans: Vec::new(),
            },
        ],
    )
    .expect("annotate");
    db
}

fn walk_query(db: &MotionDatabase) -> Vec<f32> {
    let mut q = db.query_with_mean_pose(&straight(1.5));
    q.root_velocity = Vec3::new(0.0, 0.0, 1.5);
    db.normalize(&q).expect("normalize")
}

#[test]
fn without_annotations_the_filtered_search_equals_the_plain_one() {
    let db = build(&[walk(), run()]);
    let q = walk_query(&db);
    for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
        let plain = db.search(&q, Exclusion::NONE, method);
        let filtered = db.search_with(&q, Exclusion::NONE, SearchFilter::NONE, 0.0, |_| false, method);
        assert_eq!(plain, filtered, "{method:?}");
    }
    assert!(!db.is_annotated());
    assert_eq!(db.frame_meta(0), Some(FrameMeta::default()));
    assert_eq!(db.frame_meta(db.len()), None);
}

#[test]
fn a_tag_filter_restricts_the_search_to_matching_clips() {
    let db = tagged_db();
    let q = walk_query(&db);
    let run_only = SearchFilter {
        require: 0b10,
        forbid: 0,
    };
    let no_run = SearchFilter {
        require: 0,
        forbid: 0b10,
    };
    for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
        let free = db
            .search_with(&q, Exclusion::NONE, SearchFilter::NONE, 0.0, |_| false, method)
            .expect("hit");
        assert_eq!(
            db.frame(free.frame).map(|f| f.clip),
            Some(0),
            "the walk query picks walk"
        );
        let hit = db
            .search_with(&q, Exclusion::NONE, run_only, 0.0, |_| false, method)
            .expect("hit");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(1), "{method:?}");
        let hit = db
            .search_with(&q, Exclusion::NONE, no_run, 0.0, |_| false, method)
            .expect("hit");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(0), "{method:?}");
        // A filter nothing satisfies finds nothing.
        let nothing = SearchFilter {
            require: 0b11,
            forbid: 0b01,
        };
        assert!(
            db.search_with(&q, Exclusion::NONE, nothing, 0.0, |_| false, method)
                .is_none()
        );
    }
    // A filter changes eligibility, never a cost.
    let run_hit = db
        .search_with(
            &q,
            Exclusion::NONE,
            run_only,
            0.0,
            |_| false,
            SearchMethod::BruteForce,
        )
        .expect("hit");
    assert_eq!(db.cost(&q, run_hit.frame), Some(run_hit.cost));
}

#[test]
fn pass_through_and_unindexed_frames_are_never_targets() {
    let mut db = build(&[walk()]);
    let q = walk_query(&db);
    let best = db
        .search(&q, Exclusion::NONE, SearchMethod::BruteForce)
        .expect("hit");
    let t = db.frame(best.frame).expect("frame").time;
    for kind in [SpanKind::PassThrough, SpanKind::Unindexed] {
        db.annotate(
            TagTable::default(),
            &[ClipAnnotation {
                tags: 0,
                spans: vec![Span {
                    start: (t - 0.2).max(0.0),
                    end: t + 0.2,
                    kind,
                }],
            }],
        )
        .expect("annotate");
        for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
            let hit = db
                .search_with(&q, Exclusion::NONE, SearchFilter::NONE, 0.0, |_| false, method)
                .expect("hit");
            let at = db.frame(hit.frame).expect("frame").time;
            assert!(
                at < t - 0.2 || at >= t + 0.2,
                "{kind:?} {method:?}: chose time {at} inside the span around {t}"
            );
        }
    }
}

#[test]
fn landing_bias_changes_the_winner_and_both_methods_agree() {
    let mut db = build(&[walk(), run()]);
    // A strongly negative bias on the whole run clip makes it win a walk query.
    db.annotate(
        TagTable::default(),
        &[
            ClipAnnotation::default(),
            ClipAnnotation {
                tags: 0,
                spans: vec![Span {
                    start: 0.0,
                    end: 10.0,
                    kind: SpanKind::LandingBias(-100.0),
                }],
            },
        ],
    )
    .expect("annotate");
    let q = walk_query(&db);
    for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
        let hit = db
            .search_with(&q, Exclusion::NONE, SearchFilter::NONE, 0.0, |_| false, method)
            .expect("hit");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(1), "{method:?}");
        let raw = db.cost(&q, hit.frame).expect("cost");
        assert!(
            (hit.cost - (raw - 100.0)).abs() < 1e-3,
            "biased cost {} vs {raw}",
            hit.cost
        );
    }
}

#[test]
fn the_tree_equals_brute_force_with_random_biases_filters_and_locks() {
    let mut rng = Rng::new(0x7461_6773);
    let clips = [
        walk(),
        run(),
        locomotion("turn", 2.0, 1.0, 0.4, 1.0, 1.0),
        locomotion("slow", 0.8, 0.0, 0.2, 1.2, 1.0),
    ];
    let mut db = build(&clips);
    let spans = |rng: &mut Rng| -> Vec<Span> {
        (0..3)
            .map(|_| {
                let start = rng.f32() * 0.8;
                Span {
                    start,
                    end: start + 0.05 + rng.f32() * 0.3,
                    kind: match (rng.f32() * 4.0) as u32 {
                        0 => SpanKind::PassThrough,
                        1 => SpanKind::Unindexed,
                        2 => SpanKind::Tag(1 << (rng.f32() * 3.0) as u32),
                        _ => SpanKind::LandingBias(rng.f32() * 20.0 - 14.0),
                    },
                }
            })
            .collect()
    };
    let annotations: Vec<ClipAnnotation> = (0..4)
        .map(|i| ClipAnnotation {
            tags: 1 << (i % 3),
            spans: spans(&mut rng),
        })
        .collect();
    db.annotate(TagTable::default(), &annotations).expect("annotate");
    let dims = db.dimensions();
    for case in 0..200 {
        let q: Vec<f32> = (0..dims).map(|_| rng.f32() * 4.0 - 2.0).collect();
        let filter = SearchFilter {
            require: if case % 3 == 0 { 1 << (case % 3) } else { 0 },
            forbid: if case % 5 == 0 { 0b100 } else { 0 },
        };
        let loop_bias = if case % 2 == 0 { -7.5 } else { 3.0 };
        let locked_every = 3 + case % 4;
        let locked = |row: usize| row.is_multiple_of(locked_every) && case % 7 == 0;
        let brute = db.search_with(
            &q,
            Exclusion::NONE,
            filter,
            loop_bias,
            locked,
            SearchMethod::BruteForce,
        );
        let tree = db.search_with(
            &q,
            Exclusion::NONE,
            filter,
            loop_bias,
            locked,
            SearchMethod::KdTree,
        );
        assert_eq!(brute, tree, "case {case}");
    }
}

#[test]
fn top_k_is_sorted_distinct_and_starts_with_the_best() {
    let db = tagged_db();
    let q = walk_query(&db);
    let top = db.search_top_k(&q, SearchFilter::NONE, Exclusion::NONE, 8);
    assert_eq!(top.len(), 8);
    assert!(top.windows(2).all(|w| w[0].cost <= w[1].cost));
    let best = db
        .search(&q, Exclusion::NONE, SearchMethod::BruteForce)
        .expect("hit");
    assert_eq!(top[0], best);
    let mut frames: Vec<usize> = top.iter().map(|h| h.frame).collect();
    frames.sort_unstable();
    frames.dedup();
    assert_eq!(frames.len(), 8, "distinct frames");
    assert!(
        db.search_top_k(&q, SearchFilter::NONE, Exclusion::NONE, 0)
            .is_empty()
    );
    assert_eq!(
        db.search_top_k(&q, SearchFilter::NONE, Exclusion::NONE, 500)
            .len(),
        32
    );
}

#[test]
fn annotate_checks_the_clip_count_and_the_spans_and_changes_nothing_on_error() {
    let mut db = build(&[walk(), run()]);
    assert_eq!(
        db.annotate(TagTable::default(), &[ClipAnnotation::default()]),
        Err(TagError::ClipCount { got: 1, expected: 2 })
    );
    let bad = ClipAnnotation {
        tags: 0,
        spans: vec![Span {
            start: 1.0,
            end: 0.5,
            kind: SpanKind::PassThrough,
        }],
    };
    assert!(matches!(
        db.annotate(TagTable::default(), &[ClipAnnotation::default(), bad]),
        Err(TagError::BadSpan { .. })
    ));
    assert!(!db.is_annotated());
}

fn jump_run(
    settings: MatcherSettings,
    filter: SearchFilter,
    updates: usize,
) -> (MotionMatcher, Vec<MatchStep>) {
    let db = tagged_db();
    let mut matcher = MotionMatcher::new(settings);
    matcher.filter = filter;
    let steps = (0..updates)
        .map(|i| {
            let speed = if i.is_multiple_of(2) { 1.58 } else { 1.46 };
            matcher.update(&db, &straight(speed), DT).expect("update")
        })
        .collect();
    (matcher, steps)
}

fn eager() -> MatcherSettings {
    MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.0,
        transition_cost: 0.0,
        exclude_window: 0.0,
        ..MatcherSettings::default()
    }
}

#[test]
fn a_very_negative_stay_bias_holds_the_playing_clip_and_a_positive_one_releases_it() {
    let (held, _) = jump_run(
        MatcherSettings {
            stay_bias: -1000.0,
            ..eager()
        },
        SearchFilter::NONE,
        60,
    );
    assert_eq!(
        held.jumps(),
        1,
        "only the first pick: the bias is clamped to -100, still far below any candidate"
    );
    let (released, _) = jump_run(
        MatcherSettings {
            stay_bias: 1000.0,
            ..eager()
        },
        SearchFilter::NONE,
        60,
    );
    assert!(released.jumps() > held.jumps(), "{} jumps", released.jumps());
}

#[test]
fn the_first_pick_obeys_the_filter_and_ignores_it_when_it_leaves_nothing() {
    let run_only = SearchFilter {
        require: 0b10,
        forbid: 0,
    };
    let (matcher, _) = jump_run(eager(), run_only, 30);
    let db = tagged_db();
    let playing = matcher.playback().expect("playing");
    assert_eq!(
        db.frame(playing.frame).map(|f| f.clip),
        Some(1),
        "the walk-like query is held to run"
    );
    // Nothing satisfies the filter: the first pick still happens.
    let impossible = SearchFilter {
        require: 0b11,
        forbid: 0b01,
    };
    let (matcher, steps) = jump_run(eager(), impossible, 3);
    assert!(matcher.playback().is_some());
    assert!(steps[0].jumped, "the first pick ignored the filter");
    // After that a filter with no candidates keeps the continuation.
    assert!(steps.iter().skip(1).all(|s| !s.jumped));
}

#[test]
fn a_lockout_window_keeps_a_jump_target_from_being_picked_again() {
    let settings = MatcherSettings {
        lockout_window: 100.0,
        ..eager()
    };
    let (_, steps) = jump_run(settings, SearchFilter::NONE, 90);
    let targets: Vec<usize> = steps
        .iter()
        .filter(|s| s.jumped)
        .map(|s| s.playback.frame)
        .collect();
    let mut unique = targets.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        targets.len(),
        "a locked frame was chosen twice: {targets:?}"
    );
    // Without the lockout the same run does repeat targets.
    let (_, free) = jump_run(eager(), SearchFilter::NONE, 90);
    let free_targets: Vec<usize> = free
        .iter()
        .filter(|s| s.jumped)
        .map(|s| s.playback.frame)
        .collect();
    assert!(free_targets.len() >= targets.len());
}

#[test]
fn the_loop_clip_bias_steers_toward_or_away_from_looping_clips() {
    let walk_clip = walk();
    let run_clip = run();
    let mixed = [
        MotionClip {
            clip: &walk_clip,
            looping: true,
        },
        MotionClip {
            clip: &run_clip,
            looping: false,
        },
    ];
    let db = MotionDatabase::build(&skeleton(), &mixed, schema()).expect("database");
    let q = walk_query(&db);
    for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
        // A strong penalty on looping clips sends a walk query to the non-looping run.
        let hit = db
            .search_with(&q, Exclusion::NONE, SearchFilter::NONE, 100.0, |_| false, method)
            .expect("hit");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(1), "{method:?}");
        let hit = db
            .search_with(&q, Exclusion::NONE, SearchFilter::NONE, -100.0, |_| false, method)
            .expect("hit");
        assert_eq!(db.frame(hit.frame).map(|f| f.clip), Some(0), "{method:?}");
    }
}

#[test]
fn new_settings_are_sanitized() {
    let db = tagged_db();
    let mut matcher = MotionMatcher::new(MatcherSettings {
        stay_bias: f32::NAN,
        loop_clip_bias: f32::INFINITY,
        lockout_window: -3.0,
        ..eager()
    });
    // Non-finite and negative values behave as zero: the update must not panic or poison state.
    for _ in 0..10 {
        let step = matcher.update(&db, &straight(1.5), DT).expect("update");
        assert!(step.playback.time.is_finite());
    }
}

mod conformance;
mod cooked_arrays;
mod phase_trajectory;
mod spec52;

/// A cooked range with a NaN, negative or infinite duration must not panic `frame_at` or
/// `set_playback_time`.
#[test]
fn bad_cooked_clip_durations_do_not_panic_playback_lookups() {
    let clips = [walk()];
    let sources = [MotionClip {
        clip: &clips[0],
        looping: false,
    }];
    for bad in [f32::NAN, -1.0, f32::INFINITY] {
        let mut db = MotionDatabase::build(&skeleton(), &sources, schema()).expect("database");
        db.clips[0].duration = bad;
        assert!(db.frame_at(0, 0.5).is_some());
        let mut matcher = MotionMatcher::new(MatcherSettings::default());
        matcher.playing = Some(Playback {
            clip: 0,
            time: 0.0,
            frame: 0,
        });
        assert!(matcher.set_playback_time(&db, 0.5).is_some());
    }
}

/// A schema joint beyond the player's skeleton, or a NaN clip duration, gives an empty sample.
#[test]
fn extractor_sample_survives_out_of_range_joints_and_nan_duration() {
    let sk = skeleton();
    let clips = [walk()];
    let source = MotionClip {
        clip: &clips[0],
        looping: false,
    };
    let bad_joint = FeatureSchema::new(u32::MAX, vec![FOOT_L, FOOT_R]);
    let sample = Extractor::new(&sk, &bad_joint).sample(source, 0.1);
    assert_eq!(sample.joints.len(), 2);
    let good = schema();
    let mut nan = clips[0].clone();
    nan.duration = f32::NAN;
    for looping in [false, true] {
        let source = MotionClip { clip: &nan, looping };
        let _ = Extractor::new(&sk, &good).sample(source, 0.1);
    }
}

/// audit-v035-numerics L4(b) (fix-audit-wave12): a looping cooked range of zero length made
/// `rem_euclid(0)` return NaN, stored as the playback time forever.
#[test]
fn a_zero_length_looping_range_keeps_a_finite_playback_time() {
    let mut db = build(&[walk()]);
    db.clips[0].duration = 0.0;
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 1.0e9,
        ..MatcherSettings::default()
    });
    matcher.playing = Some(Playback {
        clip: 0,
        time: 0.0,
        frame: 0,
    });
    for _ in 0..5 {
        let _ = matcher.update(&db, &straight(1.5), DT).expect("update");
        let time = matcher.playing.map(|p| p.time);
        assert!(time.is_some_and(f32::is_finite), "playback time {time:?}");
    }
}

/// audit-v035-numerics L4(a) (fix-audit-wave12): the matcher clock was f32 seconds accumulated
/// forever; at 2^24 s a step no longer advanced it, so lockout and history windows stopped
/// expiring. The clock is rebased and keeps advancing by the step.
#[test]
fn the_matcher_clock_keeps_advancing_in_a_long_session() {
    let db = build(&[walk()]);
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    let _ = matcher.update(&db, &straight(1.5), DT).expect("update");
    matcher.clock = 16_777_216.0;
    let _ = matcher.update(&db, &straight(1.5), DT).expect("update");
    let before = matcher.clock;
    let _ = matcher.update(&db, &straight(1.5), DT).expect("update");
    let advanced = matcher.clock - before;
    assert!(
        (advanced - DT).abs() < 1.0e-3,
        "the clock advanced {advanced} s for a {DT} s step"
    );
    assert!(
        matcher.recent_targets.iter().all(|&(_, at)| at <= matcher.clock),
        "recent targets moved with the clock: {:?} vs {}",
        matcher.recent_targets,
        matcher.clock
    );
}

/// The search vector written into the matcher's buffer matches decoding a query and
/// normalizing it, and the buffer's capacity stays flat across searches.
#[test]
fn search_query_matches_decode_then_normalize() {
    let db = build(&[walk(), run()]);
    let mut matcher = MotionMatcher::new(MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 10.0,
        stagger: false,
        ..MatcherSettings::default()
    });
    let path = straight(1.5);
    let mut cap_at_2 = None;
    for step in 0..8 {
        matcher.update(&db, &path, DT).expect("update");
        if step == 0 {
            // The first pick fills the vector before playback exists, then jumps.
            continue;
        }
        let query = matcher.current_query(&db, &path).expect("query");
        let normalized = db.normalize(&query).expect("normalize");
        assert_eq!(matcher.last_query(), normalized.as_slice());
        assert!(matcher.last_query.capacity() >= db.dimensions());
        if step == 1 {
            cap_at_2 = Some(matcher.last_query.capacity());
        }
    }
    assert_eq!(matcher.last_query.capacity(), cap_at_2.expect("step 2"));
}
