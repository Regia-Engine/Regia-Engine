// SPDX-License-Identifier: MPL-2.0
//! ADR-2858 acceptance: contact labels and phase groups at zero weight keep every search bit
//! identical, the foot lock cuts foot sliding, the phase cuts wrong-foot transitions, the
//! predictor drives strafes, montage scopes restrict the search, and version-1 databases load.
//! Deterministic: synthetic clips (`gait`, the spec 52 circle walk), fixed steps, no wall clock.

use super::*;
use crate::motion_matching::contacts::{FootLockSettings, phase_distance};
use crate::motion_matching::gait::{gait_set, strafe_trajectory};
use crate::motion_matching::predict::{FacingMode, TrajectoryPredictor};

fn with_contacts(mut schema: FeatureSchema) -> FeatureSchema {
    schema.contacts = Some(ContactSettings::default());
    schema
}

fn arc(times: &[f32], speed: f32, turn: f32) -> Vec<TrajectoryPoint> {
    procedural::arc_trajectory(times, speed, turn)
}

/// Every method's answer and the top 8 for `q`, as bits.
fn answers(db: &MotionDatabase, q: &[f32]) -> Vec<(usize, u32)> {
    let mut out: Vec<(usize, u32)> = [SearchMethod::BruteForce, SearchMethod::KdTree, SearchMethod::Auto]
        .into_iter()
        .filter_map(|m| db.search(q, Exclusion::NONE, m))
        .map(|h| (h.frame, h.cost.to_bits()))
        .collect();
    out.extend(
        db.search_top_k(q, SearchFilter::NONE, Exclusion::NONE, 8)
            .into_iter()
            .map(|h| (h.frame, h.cost.to_bits())),
    );
    out
}

/// The spec 52 trace path (walk, run, turn, walk; a copy of `spec52.rs`'s).
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

fn trace(db: &MotionDatabase, settings: MatcherSettings) -> String {
    let mut matcher = MotionMatcher::new(settings);
    let mut out = String::new();
    for step in 0..200 {
        let result = matcher.update(db, &trace_path(step), DT).expect("update");
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
fn zero_weight_contact_and_phase_groups_keep_every_search_bit_identical() {
    let skeleton = skeleton();
    let clip_sets: Vec<Vec<AnimationClip>> = vec![
        vec![walk(), run(), locomotion("turn", 2.0, 1.0, 0.4, 1.0, 1.0)],
        vec![
            locomotion("straight", 1.5, 0.0, 0.3, 1.0, 2.0),
            locomotion("turn_pos", 1.5, 0.75, 0.3, 1.0, 2.0),
            locomotion("turn_neg", 1.5, -0.75, 0.3, 1.0, 2.0),
        ],
    ];
    let mut schemas = vec![schema()];
    let mut history = schema();
    history.history_times = vec![0.25, 0.5];
    schemas.push(history);
    for clips in &clip_sets {
        for base in &schemas {
            let old = MotionDatabase::build(&skeleton, &looping(clips), base.clone()).expect("db");
            let new =
                MotionDatabase::build(&skeleton, &looping(clips), with_contacts(base.clone())).expect("db");
            assert_eq!(
                new.dimensions(),
                old.dimensions() + 6,
                "two feet: 1 contact + 2 phase each"
            );
            // The baseline floats are untouched and come first.
            for frame in 0..old.len() {
                let (a, b) = (
                    old.raw_features(frame).expect("f"),
                    new.raw_features(frame).expect("f"),
                );
                assert_eq!(&b[..a.len()], a, "frame {frame}");
            }
            assert_eq!(&new.mean()[..old.dimensions()], old.mean());
            assert_eq!(&new.scale()[..old.dimensions()], old.scale());
            for frame in (0..old.len()).step_by(3) {
                for path in [
                    straight(1.4),
                    straight(3.8),
                    arc(&DEFAULT_TRAJECTORY_TIMES, 2.0, 0.9),
                    arc(&DEFAULT_TRAJECTORY_TIMES, 1.5, -0.75),
                ] {
                    let qa = old
                        .normalize(&old.query_from_frame(frame, &path).expect("q"))
                        .expect("n");
                    let qb = new
                        .normalize(&new.query_from_frame(frame, &path).expect("q"))
                        .expect("n");
                    assert_eq!(answers(&old, &qa), answers(&new, &qb), "frame {frame}");
                    let pa = old.cost_parts(&qa, frame + 1).map(|p| p.total.to_bits());
                    let pb = new.cost_parts(&qb, frame + 1).map(|p| p.total.to_bits());
                    assert_eq!(pa, pb);
                }
            }
        }
    }
    // The spec 52 regression pins (`default_jump_rule_equals_the_baseline_trace`) hold with the
    // new groups present at zero weight.
    let clips = &clip_sets[0];
    let old = MotionDatabase::build(&skeleton, &looping(clips), schema()).expect("db");
    let new = MotionDatabase::build(&skeleton, &looping(clips), with_contacts(schema())).expect("db");
    let quick = MatcherSettings {
        search_interval: 0.0,
        min_jump_interval: 0.1,
        transition_cost: 0.02,
        ..MatcherSettings::default()
    };
    for settings in [MatcherSettings::default(), quick] {
        assert_eq!(trace(&old, settings), trace(&new, settings));
    }
    assert_eq!(
        trace(&new, MatcherSettings::default()),
        "0:0:0,102:2:73,150:1:46,"
    );
    assert_eq!(trace(&new, quick), "0:0:0,100:2:71,150:1:46,");
}

#[test]
fn a_schema_without_contacts_keeps_the_v1_layout_and_cooked_body() {
    let db = build(&[walk(), run()]);
    assert_eq!(db.schema().contacts, None);
    assert_eq!(db.dimensions(), 3 + 12 + 12);
    assert!(db.contacts(0).expect("frame").is_empty());
    assert!(db.phases(0).expect("frame").is_empty());
    // The query of a frame has no contact or phase parts; a stray one is a shape error.
    let mut q = db.frame_query(0).expect("q");
    assert!(q.contacts.is_empty() && q.phases.is_empty());
    q.contacts.push(1.0);
    assert!(matches!(
        db.normalize(&q),
        Err(MotionMatchError::QueryShape { part: "contacts", .. })
    ));
}

/// What one scripted run did.
struct GaitRun {
    /// `(from frame, to frame)` per jump after the first pick.
    jumps: Vec<(usize, usize)>,
    /// Horizontal world distance the feet moved while labelled planted (two planted frames in a
    /// row), meters.
    sliding: f32,
    /// Planted foot-frames counted.
    planted: usize,
    /// Tags of the playing frame at each step.
    tags: Vec<u32>,
}

/// Drive the gait database for `steps` fixed steps: the predictor turns `desired(step)` into the
/// path, the matcher and the player run, the character moves by the root motion, and the feet
/// (through the foot lock when `lock`) are measured.
fn drive_gait(
    db: Arc<MotionDatabase>,
    settings: MatcherSettings,
    lock: bool,
    steps: usize,
    desired: impl Fn(usize) -> (Vec3, FacingMode, f32),
) -> GaitRun {
    let set = gait_set();
    let clips: Arc<[AnimationClip]> = set.entry_clips().into();
    let mut player = MotionMatchPlayer::new(clips, Arc::new(set.skeleton.clone()));
    let mut matching = MotionMatching::new(db.clone(), settings);
    let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
    let mut locker = FootLocker::new(FootLockSettings::default());
    let (mut position, mut yaw) = (Vec3::ZERO, 0.0f32);
    let mut previous: Option<(Vec<bool>, Vec<Vec3>)> = None;
    let mut run = GaitRun {
        jumps: Vec::new(),
        sliding: 0.0,
        planted: 0,
        tags: Vec::new(),
    };
    for step in 0..steps {
        let (velocity, facing, aim) = desired(step);
        predictor.desired_velocity = velocity;
        predictor.facing = facing;
        predictor.aim_yaw = aim;
        predictor.drive(&mut matching, DT);
        let result = matching.step(DT).expect("step");
        if let Some(jump) = result.jump
            && let Some(from) = jump.from
        {
            run.jumps.push((from.frame, jump.to.frame));
        }
        step_player(&mut matching, &mut player, DT);
        let delta = player.root_motion();
        position += Quat::from_rotation_y(yaw) * delta.translation;
        yaw += delta.yaw;
        let to_world = |p: Vec3| position + Quat::from_rotation_y(yaw) * p;
        let animated: Vec<Vec3> = contacts::player_feet(&db, &player)
            .into_iter()
            .map(to_world)
            .collect();
        let planted = db.contacts(result.playback.frame).expect("frame");
        run.tags
            .push(db.frame_meta(result.playback.frame).expect("meta").tags);
        let locks = locker.update(&planted, &animated, DT).to_vec();
        let feet: Vec<Vec3> = if lock {
            locks
                .iter()
                .zip(&animated)
                .map(|(l, &a)| l.ik_target(a))
                .collect()
        } else {
            animated
        };
        if let Some((was, before)) = &previous {
            for (((&now_down, &was_down), now), then) in planted.iter().zip(was).zip(&feet).zip(before) {
                if now_down && was_down {
                    let d = *now - *then;
                    run.sliding += Vec2::new(d.x, d.z).length();
                    run.planted += 1;
                }
            }
        }
        previous = Some((planted, feet));
    }
    run
}

/// Speed and direction changes every 1.5 s: walk, fast walk, a left curve, a slow walk, a strafe.
fn locomotion_script(step: usize) -> (Vec3, FacingMode, f32) {
    let t = step as f32 * DT;
    match (step / 45) % 5 {
        0 => (Vec3::new(0.0, 0.0, 1.5), FacingMode::Velocity, 0.0),
        1 => (Vec3::new(0.0, 0.0, 2.0), FacingMode::Velocity, 0.0),
        2 => {
            let a = -(t - 3.0);
            (
                Vec3::new(1.5 * a.sin(), 0.0, 1.5 * a.cos()),
                FacingMode::Velocity,
                0.0,
            )
        }
        3 => (Vec3::new(0.0, 0.0, 1.0), FacingMode::Velocity, 0.0),
        _ => (Vec3::new(1.2, 0.0, 0.0), FacingMode::Aim, 0.0),
    }
}

fn gait_db(weights: impl FnOnce(&mut FeatureWeights)) -> Arc<MotionDatabase> {
    let mut set = gait_set();
    let mut w: FeatureWeights = set.source.schema.weights.into();
    weights(&mut w);
    set.source.schema.weights = asset::WeightsSource {
        root_velocity: w.root_velocity,
        trajectory_position: w.trajectory_position,
        trajectory_direction: w.trajectory_direction,
        foot_position: w.foot_position,
        foot_velocity: w.foot_velocity,
        trajectory_history: w.trajectory_history,
        pose_position: w.pose_position,
        pose_velocity: w.pose_velocity,
        foot_contact: w.foot_contact,
        foot_phase: w.foot_phase,
    };
    Arc::new(set.build().expect("gait database"))
}

#[test]
fn contact_labels_and_the_foot_lock_cut_foot_sliding() {
    let settings = MatcherSettings::default();
    let steps = 600;
    let baseline = drive_gait(gait_db(|_| {}), settings, false, steps, locomotion_script);
    let search_only = drive_gait(
        gait_db(|w| w.foot_contact = 1.0),
        settings,
        false,
        steps,
        locomotion_script,
    );
    let locked = drive_gait(
        gait_db(|w| w.foot_contact = 1.0),
        settings,
        true,
        steps,
        locomotion_script,
    );
    eprintln!(
        "foot sliding over {steps} steps: baseline {:.4} m ({} planted foot-frames, {} jumps), \
         contact weight 1 {:.4} m ({} jumps), contact weight 1 + foot lock {:.4} m",
        baseline.sliding,
        baseline.planted,
        baseline.jumps.len(),
        search_only.sliding,
        search_only.jumps.len(),
        locked.sliding
    );
    assert!(
        baseline.planted > 300,
        "the script plants feet: {}",
        baseline.planted
    );
    assert!(
        baseline.sliding > 0.05,
        "the baseline slides: {}",
        baseline.sliding
    );
    // Pinned: at least a 2x reduction (measured on this fixture: see the report and ADR-2858).
    assert!(
        locked.sliding * 2.0 <= baseline.sliding,
        "{} vs {}",
        locked.sliding,
        baseline.sliding
    );
    // Deterministic: the same script gives the same numbers.
    let again = drive_gait(
        gait_db(|w| w.foot_contact = 1.0),
        settings,
        true,
        steps,
        locomotion_script,
    );
    assert_eq!(again.sliding.to_bits(), locked.sliding.to_bits());
}

/// Straight walking whose speed changes every 0.6 s (1.0, 1.5, 2.0, 1.5 m/s).
fn speed_script(step: usize) -> (Vec3, FacingMode, f32) {
    let speed = [1.0, 1.5, 2.0, 1.5][(step / 18) % 4];
    (Vec3::new(0.0, 0.0, speed), FacingMode::Velocity, 0.0)
}

fn wrong_foot_jumps(db: &MotionDatabase, jumps: &[(usize, usize)]) -> usize {
    jumps
        .iter()
        .filter(|&&(from, to)| {
            let (a, b) = (db.phases(from).expect("f"), db.phases(to).expect("f"));
            match (a.first().copied().flatten(), b.first().copied().flatten()) {
                (Some(p), Some(q)) => phase_distance(p, q) > 0.25,
                _ => false,
            }
        })
        .count()
}

#[test]
fn the_phase_group_cuts_wrong_foot_transitions_on_mirrored_gaits() {
    // Path-driven tuning: no foot position or velocity weight, so the pose features alone do
    // not say which foot is down (the walks and their mirrored copies tie on the path).
    let path_driven = |w: &mut FeatureWeights| {
        w.foot_position = 0.0;
        w.foot_velocity = 0.0;
    };
    let settings = MatcherSettings::default();
    let steps = 720;
    let off = gait_db(path_driven);
    let on = gait_db(|w| {
        path_driven(w);
        w.foot_phase = 1.0;
    });
    let base = drive_gait(off.clone(), settings, false, steps, speed_script);
    let phased = drive_gait(on.clone(), settings, false, steps, speed_script);
    let (wrong_off, wrong_on) = (
        wrong_foot_jumps(&off, &base.jumps),
        wrong_foot_jumps(&on, &phased.jumps),
    );
    eprintln!(
        "wrong-foot jumps over {steps} steps: phase weight 0: {wrong_off} of {}, phase weight 1: {wrong_on} of {}",
        base.jumps.len(),
        phased.jumps.len()
    );
    assert!(
        wrong_off >= 4,
        "the fixture must produce wrong-foot jumps without the phase: {wrong_off}"
    );
    assert!(wrong_on * 2 <= wrong_off, "{wrong_on} vs {wrong_off}");
}

/// The mm_locomotion sample's authored weights: the path counts more than the pose.
fn path_weights(w: &mut FeatureWeights) {
    w.trajectory_position = 4.0;
    w.trajectory_direction = 2.0;
    w.foot_position = 0.5;
    w.foot_velocity = 0.25;
}

#[test]
fn strafing_at_constant_facing_picks_the_strafe_clips() {
    // Path-driven tuning (the mm_locomotion sample's weights): with equal weights the idle pose
    // features keep a standing character in idle whatever the path asks.
    let db = gait_db(path_weights);
    let strafe = db.tag_table().mask(&["strafe"]).expect("tag");
    let right = |_: usize| (Vec3::new(1.2, 0.0, 0.0), FacingMode::Aim, 0.0);
    let run = drive_gait(db.clone(), MatcherSettings::default(), false, 90, right);
    let strafing = run.tags[30..].iter().filter(|&&t| t & strafe != 0).count();
    assert_eq!(
        strafing,
        60,
        "after a second every frame is a strafe: {:?}",
        &run.tags[30..]
    );
    // Facing the velocity instead turns toward +X: no strafe.
    let turn = |_: usize| (Vec3::new(1.2, 0.0, 0.0), FacingMode::Velocity, 0.0);
    let run = drive_gait(db.clone(), MatcherSettings::default(), false, 90, turn);
    assert!(run.tags.iter().all(|&t| t & strafe == 0), "{:?}", run.tags);
    // The hand-written strafe path agrees with the predictor's.
    let path = strafe_trajectory(&DEFAULT_TRAJECTORY_TIMES, Vec2::new(1.2, 0.0));
    let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
    predictor.facing = FacingMode::Aim;
    predictor.desired_velocity = Vec3::new(1.2, 0.0, 0.0);
    predictor.state.velocity = predictor.desired_velocity;
    for (a, b) in path.iter().zip(predictor.trajectory(&DEFAULT_TRAJECTORY_TIMES)) {
        assert!(a.position.distance(b.position) < 1e-4 && a.direction.distance(b.direction) < 1e-4);
    }
}

#[test]
fn a_montage_scope_only_returns_its_tagged_frames() {
    let db = gait_db(|_| {});
    let strafe = db.tag_table().mask(&["strafe"]).expect("tag");
    let forward = straight(1.5);
    let mut matcher = MotionMatcher::new(MatcherSettings::default());
    matcher.scope = Some(SearchFilter {
        require: strafe,
        forbid: 0,
    });
    for _ in 0..60 {
        let step = matcher.update(&db, &forward, DT).expect("update");
        let tags = db.frame_meta(step.playback.frame).expect("meta").tags;
        assert!(tags & strafe != 0, "scoped playback stays on strafe frames");
        if let Some(best) = matcher.last_search().and_then(|s| s.best) {
            assert!(db.frame_meta(best.frame).expect("m").tags & strafe != 0);
        }
    }
    // Leaving the scope lets the forward path pick a walk again.
    matcher.scope = None;
    let mut walked = false;
    for _ in 0..30 {
        let step = matcher.update(&db, &forward, DT).expect("update");
        walked |= db.frame_meta(step.playback.frame).expect("m").tags & strafe == 0;
    }
    assert!(walked);
    // An impossible scope keeps the continuation and says why.
    matcher.scope = Some(SearchFilter {
        require: strafe,
        forbid: strafe,
    });
    matcher.request_search();
    let step = matcher.update(&db, &forward, DT).expect("update");
    assert!(!step.jumped);
    assert_eq!(
        matcher.last_search().map(|s| s.reason),
        Some(SearchReason::NoCandidates)
    );
    // The scope and the matcher's own filter combine.
    matcher.filter = SearchFilter {
        require: 0,
        forbid: 1,
    };
    matcher.scope = Some(SearchFilter {
        require: strafe,
        forbid: 0,
    });
    assert_eq!(
        matcher.effective_filter(),
        SearchFilter {
            require: strafe,
            forbid: 1
        }
    );
}

#[test]
fn a_v1_source_loads_and_upgrades_to_v2_unchanged() {
    let v1 = gait_set().source.to_ron_string().expect("write");
    let v1 = v1
        .replace("format_version: 2", "format_version: 1")
        .replace("contacts: Some", "contacts_removed: Some");
    // A real v1 file has no `contacts` and no new weights: strip them.
    let v1: String = v1
        .lines()
        .filter(|l| !l.contains("foot_contact") && !l.contains("foot_phase"))
        .collect::<Vec<_>>()
        .join("\n");
    let v1 = strip_field(&v1, "contacts_removed");
    let parsed = MotionDatabaseSource::from_ron_str(&v1).expect("v1 parses");
    assert_eq!(parsed.format_version, MOTIONDB_FORMAT_VERSION);
    assert_eq!(parsed.schema.contacts, None);
    assert_eq!(parsed.schema.weights.foot_contact, 0.0);
    let upgraded = asset::migrate_motiondb(&v1).expect("migrates");
    assert!(upgraded.contains("format_version: 2"), "{upgraded}");
    let again = MotionDatabaseSource::from_ron_str(&upgraded).expect("v2 parses");
    assert_eq!(again, parsed);
    // A v1 source and its upgrade build the same database (bit for bit).
    let set = gait_set();
    let lookup = |p: &str| set.clip(p);
    let a = parsed.build(&set.skeleton, lookup).expect("build");
    let b = again.build(&set.skeleton, lookup).expect("build");
    assert_eq!(
        a.to_cooked_payload().expect("c"),
        b.to_cooked_payload().expect("c")
    );
}

/// Remove a `name: Some((...)),` field (balanced parentheses) from RON text.
fn strip_field(text: &str, name: &str) -> String {
    let Some(start) = text.find(name) else {
        return text.to_string();
    };
    let mut depth = 0usize;
    let mut end = start;
    for (i, c) in text[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let rest = text[end..].trim_start_matches(',');
    format!("{}{}", &text[..start], rest)
}

#[test]
fn cooked_contacts_round_trip_and_a_v1_payload_loads_bit_for_bit() {
    let db = gait_db(|w| {
        w.foot_contact = 0.5;
        w.foot_phase = 2.0;
    });
    let bytes = db.to_cooked_payload().expect("cook");
    let back = MotionDatabase::from_cooked_payload(&bytes).expect("load");
    assert_eq!(back.schema(), db.schema());
    for frame in (0..db.len()).step_by(7) {
        assert_eq!(back.contacts(frame), db.contacts(frame));
        assert_eq!(back.features(frame), db.features(frame));
    }
    // A database without contacts cooked as version 1 (the old layout) loads as the same
    // database.
    let plain = build(&[walk(), run()]);
    let v1 = plain.to_cooked_payload_v1().expect("v1 cook");
    assert_eq!(MotionDatabase::cooked_payload_version(&v1), Ok(1));
    // check-formats trusts this list: every registered source must load.
    assert_eq!(
        crate::motion_matching::cooked_motiondb_migration_sources(),
        vec![1, 2]
    );
    let loaded = MotionDatabase::from_cooked_payload(&v1).expect("v1 loads");
    assert_eq!(loaded.schema(), plain.schema());
    assert_eq!(
        loaded.to_cooked_payload().expect("c"),
        plain.to_cooked_payload().expect("c")
    );
    let q = plain
        .normalize(&plain.query_from_frame(5, &straight(3.0)).expect("q"))
        .expect("n");
    assert_eq!(answers(&loaded, &q), answers(&plain, &q));
}

#[test]
fn the_plugin_predicts_scopes_and_locks_feet() {
    use crate::montage_player::MontagePlayer;
    let mut app = regia_app::App::new();
    app.add_plugins(regia_app::MinimalPlugins);
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 30.0),
    ));
    app.add_plugins(MotionMatchPlugin);
    let db = gait_db(|w| w.foot_contact = 1.0);
    let set = gait_set();
    let clips: Arc<[AnimationClip]> = set.entry_clips().into();
    let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
    predictor.desired_velocity = Vec3::new(0.0, 0.0, 1.5);
    let entity = app
        .world_mut()
        .spawn((
            MotionMatching::new(db.clone(), MatcherSettings::default()),
            MotionMatchPlayer::new(clips, Arc::new(set.skeleton.clone())),
            predictor,
            MotionMatchFootLocks::default(),
            MotionMatchMontageScope::default(),
            MontagePlayer::default(),
        ))
        .id();
    for _ in 0..30 {
        app.update();
    }
    let world = app.world();
    let matching = world.get::<MotionMatching>(entity).expect("matching");
    // The predictor wrote the path: about 1.5 m/s straight ahead.
    let far = matching.trajectory.last().expect("point");
    assert!((far.position.y - 1.5).abs() < 0.2, "{far:?}");
    let walk = db.tag_table().mask(&["walk"]).expect("tag");
    let frame = matching.matcher.playback().expect("playing").frame;
    assert!(db.frame_meta(frame).expect("m").tags & walk != 0);
    // One lock per foot, at least one planted.
    let locks = world.get::<MotionMatchFootLocks>(entity).expect("locks").locks();
    assert_eq!(locks.len(), 2);
    assert!(locks.iter().any(|l| l.planted && l.weight == 1.0), "{locks:?}");
    // No montage plays: no scope.
    assert_eq!(matching.matcher.scope, None);
}

#[test]
#[ignore = "hand-run timing; run in release with --ignored --nocapture"]
fn perf_micro_phase_trajectory() {
    use std::time::Instant;

    // 20 clips of 20 s at 30 Hz (12,000 frames), with and without contacts and phase.
    let clips: Vec<AnimationClip> = (0..20)
        .map(|i| locomotion(&format!("clip{i}"), 0.5 + 0.2 * i as f32, 0.0, 0.3, 1.0, 20.0))
        .collect();
    let time = |f: &dyn Fn() -> MotionDatabase| {
        let started = Instant::now();
        let db = f();
        (db, started.elapsed().as_secs_f64() * 1e3)
    };
    let (plain, plain_ms) =
        time(&|| MotionDatabase::build(&skeleton(), &looping(&clips), schema()).expect("db"));
    let (labelled, labelled_ms) =
        time(&|| MotionDatabase::build(&skeleton(), &looping(&clips), with_contacts(schema())).expect("db"));
    println!(
        "perf-micro mm_phase_build: frames={} plain_ms={plain_ms:.1} contacts_phase_ms={labelled_ms:.1}",
        plain.len()
    );
    for (name, db) in [("plain", &plain), ("contacts_zero_weight", &labelled)] {
        let times: Vec<f64> = (0..300)
            .map(|i| {
                let q = db
                    .normalize(
                        &db.query_from_frame((i * 37) % db.len(), &straight(1.0 + (i % 7) as f32 * 0.4))
                            .expect("q"),
                    )
                    .expect("n");
                let started = Instant::now();
                let hit = db.search(&q, Exclusion::NONE, SearchMethod::Auto);
                let us = started.elapsed().as_secs_f64() * 1e6;
                assert!(hit.is_some());
                us
            })
            .collect();
        let (p50, p99, max) = percentiles(times);
        println!(
            "perf-micro mm_phase_search_{name}: frames={} dims={} p50={p50:.1}us p99={p99:.1}us max={max:.1}us",
            db.len(),
            db.dimensions()
        );
    }
    // The predictor: advance + a three-point path, per character per frame.
    let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
    let times: Vec<f64> = (0..2000)
        .map(|i| {
            predictor.desired_velocity = Vec3::new((i as f32 * 0.01).sin(), 0.0, 1.5);
            let started = Instant::now();
            predictor.advance(DT);
            let path = predictor.trajectory(&DEFAULT_TRAJECTORY_TIMES);
            let us = started.elapsed().as_secs_f64() * 1e6;
            assert_eq!(path.len(), 3);
            us
        })
        .collect();
    let (p50, p99, max) = percentiles(times);
    println!("perf-micro mm_phase_predictor: p50={p50:.2}us p99={p99:.2}us max={max:.2}us");
    // The foot locker, two feet.
    let mut locker = FootLocker::default();
    let started = Instant::now();
    for i in 0..10_000 {
        let down = i % 30 < 18;
        locker.update(
            &[down, !down],
            &[Vec3::new(i as f32 * 0.01, 0.0, 0.0), Vec3::ZERO],
            DT,
        );
    }
    println!(
        "perf-micro mm_phase_foot_lock: per_update_us={:.3}",
        started.elapsed().as_secs_f64() * 1e6 / 10_000.0
    );
}
