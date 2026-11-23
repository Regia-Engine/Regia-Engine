// SPDX-License-Identifier: MPL-2.0
//! The optional cooked arc-length and clip-speed arrays (`docs/specs/52-motion-matching.md` § 4,
//! ADR-7817): a cook stores them, the runtime reads them when present and computes the same
//! values when absent, and older cooked payloads migrate to the current version.
//! Deterministic: fixed clips and fixed steps; the only timing is the ignored hand-run test.

use super::spec52::{SOURCE, source_clips, stop_clip};
use super::*;

fn lookup<'a>(clips: &'a [(String, AnimationClip)]) -> impl Fn(&str) -> Option<&'a AnimationClip> {
    move |path: &str| clips.iter().find(|(p, _)| p == path).map(|(_, c)| c)
}

/// The spec 52 test source (walk, run, a distance-matched stop).
fn source_db() -> MotionDatabase {
    let source = MotionDatabaseSource::from_ron_str(SOURCE).expect("parse");
    let clips = source_clips();
    source.build(&skeleton(), lookup(&clips)).expect("build")
}

/// The source clips in entry order (what a player plays, [`ClipRange::source`]).
fn entry_clips() -> Vec<AnimationClip> {
    source_clips().into_iter().map(|(_, c)| c).collect()
}

/// A stop (distance matched, mirrored too) and a walk: every jump lands in a stop range or the
/// walk, and the stop ranges carry arc-length arrays.
const STOP_SOURCE: &str = r#"(
  format_version: 2,
  skeleton: "characters/test.skeleton",
  schema: ( root_joint: "root", feet: ["foot_l", "foot_r"] ),
  mirror_pairs: [("foot_l", "foot_r")],
  entries: [
    ( clip: "anim/stop.clip", distance_matched: true, mirrored: true ),
    ( clip: "anim/walk.clip", looping: true ),
  ],
)"#;

fn stop_db() -> (MotionDatabase, Vec<AnimationClip>) {
    let clips = vec![
        ("anim/stop.clip".to_string(), stop_clip()),
        ("anim/walk.clip".to_string(), walk()),
    ];
    let source = MotionDatabaseSource::from_ron_str(STOP_SOURCE).expect("parse");
    let db = source.build(&skeleton(), lookup(&clips)).expect("build");
    (db, clips.into_iter().map(|(_, c)| c).collect())
}

#[test]
fn the_cooked_payload_carries_the_clip_speed_and_arc_length_arrays() {
    assert_eq!(COOKED_MOTIONDB_VERSION, 3);
    let db = source_db();
    let payload = db.to_cooked_payload().expect("cook");
    assert_eq!(
        MotionDatabase::cooked_payload_version(&payload).expect("version"),
        3
    );
    let loaded = MotionDatabase::from_cooked_payload(&payload).expect("load");
    let speeds = loaded.clip_speeds();
    assert_eq!(speeds.len(), loaded.len());
    for (frame, &speed) in speeds.iter().enumerate() {
        let raw = loaded.raw_features(frame).expect("raw");
        let expected = (raw[0] * raw[0] + raw[2] * raw[2]).sqrt();
        assert_eq!(speed.to_bits(), expected.to_bits(), "frame {frame}");
        assert_eq!(
            loaded.clip_speed(frame).map(f32::to_bits),
            Some(expected.to_bits())
        );
    }
    // Only the distance-matched stop has an arc-length table.
    assert!(loaded.cooked_arc_lengths(0).is_none());
    assert!(loaded.cooked_arc_lengths(1).is_none());
    let stop = loaded.cooked_arc_lengths(2).expect("the stop's arc lengths");
    let frames = loaded.clips()[2].frames;
    assert_eq!((stop.distances.len(), stop.speeds.len()), (frames, frames));
    assert_eq!(stop.distances[0], 0.0);
    assert!(stop.distances.windows(2).all(|w| w[1] >= w[0]), "monotone");
    close(
        *stop.distances.last().expect("last"),
        1.0,
        2e-3,
        "the stop travels 1 m",
    );
    assert!(stop.speeds.iter().all(|s| s.is_finite() && *s >= 0.0));
    // The arrays round-trip bit for bit, and the cook stays deterministic.
    assert_eq!(loaded.to_cooked_payload().expect("cook"), payload);
    assert_eq!(source_db().to_cooked_payload().expect("cook"), payload);
}

/// Everything a run of the player shows, as bits: playback, stride ratio, pose, root motion.
fn player_trace(db: Arc<MotionDatabase>, clips: &[AnimationClip]) -> Vec<Vec<u32>> {
    let mut matching = MotionMatching::new(db.clone(), MatcherSettings::default());
    matching.trajectory = straight(0.5);
    let mut player = MotionMatchPlayer::new(Arc::from(clips.to_vec()), Arc::new(skeleton()));
    player.warps.stride.strength = 1.0;
    player.warp_inputs.actual_speed = Some(1.2);
    let mut trace = Vec::new();
    for step in 0..90u32 {
        // The target comes closer, then moves away for a while (playback holds), then arrives.
        let distance = match step {
            0..=39 => 1.0 - step as f32 * 0.02,
            40..=49 => 0.4,
            _ => (0.2 - (step - 50) as f32 * 0.01).max(0.0),
        };
        player.warp_inputs.distance_to_target = Some(distance);
        matching.step(DT).expect("step");
        step_player(&mut matching, &mut player, DT);
        let playback = matching.matcher.playback().expect("playback");
        let mut row = vec![
            playback.clip as u32,
            playback.frame as u32,
            playback.time.to_bits(),
            player.stride_ratio().to_bits(),
            player.root_motion().translation.z.to_bits(),
        ];
        let pose = player.pose();
        row.extend(
            pose.translations
                .iter()
                .flat_map(|t| t.to_array())
                .map(f32::to_bits),
        );
        row.extend(pose.rotations.iter().flat_map(|r| r.to_array()).map(f32::to_bits));
        trace.push(row);
    }
    trace
}

#[test]
fn the_runtime_gives_identical_results_with_and_without_the_arrays() {
    let (db, clips) = stop_db();
    assert!(db.clips()[0].distance_matched && db.clips()[1].distance_matched);
    let with = MotionDatabase::from_cooked_payload(&db.to_cooked_payload().expect("cook")).expect("v3");
    let without =
        MotionDatabase::from_cooked_payload(&db.to_cooked_payload_v2().expect("cook v2")).expect("v2");
    assert!(with.cooked_arc_lengths(0).is_some() && with.cooked_arc_lengths(1).is_some());
    assert!(without.cooked_arc_lengths(0).is_none() && without.cooked_arc_lengths(1).is_none());
    // The v2 payload has no clip-speed array; the loader computes it from the raw features.
    assert_eq!(with.clip_speeds(), without.clip_speeds());
    // The clip speed and the arc-length tables are the same bits either way.
    for frame in 0..with.len() {
        assert_eq!(
            with.clip_speed(frame).map(f32::to_bits),
            without.clip_speed(frame).map(f32::to_bits),
            "frame {frame}"
        );
    }
    let skeleton = Arc::new(skeleton());
    let shared: Arc<[AnimationClip]> = Arc::from(clips.clone());
    for stop_at_end in [false, true] {
        for threshold in [0.05, 0.5] {
            let mut a = MotionMatchPlayer::new(shared.clone(), skeleton.clone());
            let mut b = MotionMatchPlayer::new(shared.clone(), skeleton.clone());
            for p in [&mut a, &mut b] {
                p.warps.stop_at_end = stop_at_end;
                p.warps.stop_speed_threshold = threshold;
            }
            for clip in 0..with.clips().len() {
                let cooked = a.arc_table(&with, clip).cloned();
                let sampled = b.arc_table(&without, clip).cloned();
                assert_eq!(
                    cooked, sampled,
                    "clip {clip}, stop_at_end {stop_at_end}, {threshold}"
                );
                assert_eq!(
                    cooked.is_some(),
                    clip < 2,
                    "only the stop ranges are distance matched"
                );
            }
        }
    }
    // A whole run with distance matching and the stride warp shows the same bits.
    let traced = player_trace(Arc::new(with), &clips);
    assert!(traced.iter().any(|row| row[0] < 2), "the run plays a stop range");
    assert_eq!(traced, player_trace(Arc::new(without), &clips));
}

#[test]
fn an_older_cooked_database_migrates_to_the_current_version() {
    let db = source_db();
    let fresh = db.to_cooked_payload().expect("cook");
    let fresh_db = MotionDatabase::from_cooked_payload(&fresh).expect("load");
    for (version, old) in [
        (1, db.to_cooked_payload_v1().expect("v1 layout")),
        (2, db.to_cooked_payload_v2().expect("v2 layout")),
    ] {
        assert_eq!(MotionDatabase::cooked_payload_version(&old).expect("v"), version);
        let migrated = migrate_cooked_motiondb(&old).expect("migrate");
        assert_eq!(
            MotionDatabase::cooked_payload_version(&migrated).expect("v"),
            COOKED_MOTIONDB_VERSION
        );
        let mut upgraded = MotionDatabase::from_cooked_payload(&migrated).expect("load");
        // The payload alone gives the clip speeds; the arc lengths need the clips.
        assert_eq!(upgraded.clip_speeds(), fresh_db.clip_speeds());
        assert!(upgraded.cooked_arc_lengths(2).is_none());
        // Searches are unchanged.
        let q = fresh_db
            .normalize(&fresh_db.frame_query(10).expect("query"))
            .expect("normalize");
        assert_eq!(
            upgraded.search(&q, Exclusion::NONE, SearchMethod::BruteForce),
            fresh_db.search(&q, Exclusion::NONE, SearchMethod::BruteForce)
        );
        // Migrating again changes no byte.
        assert_eq!(migrate_cooked_motiondb(&migrated).expect("again"), migrated);
        // With the clips the arc lengths are baked too, and the payload equals a fresh cook.
        upgraded
            .bake_arc_lengths(&skeleton(), &entry_clips())
            .expect("bake");
        assert_eq!(
            upgraded.to_cooked_payload().expect("cook"),
            fresh,
            "from v{version}"
        );
    }
    // A clip list that misses an entry is an error, not a panic.
    let mut short = fresh_db.clone();
    assert!(matches!(
        short.bake_arc_lengths(&skeleton(), &entry_clips()[..1]),
        Err(MotionMatchError::UnknownClip { entry: 2, .. })
    ));
    // A newer payload is refused by the migrator as by the loader.
    assert!(migrate_cooked_motiondb(&[0u8; 4]).is_err());
}

#[test]
fn a_cooked_file_migrates_with_its_header_kept() {
    let db = source_db();
    let header = |payload: &[u8]| regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: crate::cooked::motion_database_type_tag(),
        asset_id: regia_asset::AssetId::default(),
        importer_version: 3,
        dependencies: vec![regia_asset::AssetId::sub_asset(
            regia_asset::AssetId::default(),
            "s",
        )],
        payload_len: payload.len() as u64,
    };
    let old = db.to_cooked_payload_v2().expect("v2");
    let file = regia_asset::encode_cooked_file(&header(&old), &old).expect("encode");
    assert_eq!(
        crate::cooked::cooked_motion_database_file_version(&file).expect("probe"),
        Some(2)
    );
    let migrated = crate::cooked::migrate_cooked_motion_database_file(&file).expect("migrate");
    assert_eq!(
        crate::cooked::cooked_motion_database_file_version(&migrated).expect("probe"),
        Some(COOKED_MOTIONDB_VERSION)
    );
    let (kept, loaded) = crate::cooked::load_cooked_motion_database(&migrated).expect("load");
    let expected = header(&migrate_cooked_motiondb(&old).expect("payload"));
    assert_eq!(
        kept, expected,
        "the header is kept, only the payload length follows"
    );
    assert_eq!(loaded.clip_speeds().len(), loaded.len());
    // Another cooked type is not a motion database.
    let clip = stop_clip().to_cooked_payload().expect("clip");
    let other = regia_asset::CookedHeader {
        type_tag: crate::cooked::clip_type_tag(),
        ..header(&clip)
    };
    let clip_file = regia_asset::encode_cooked_file(&other, &clip).expect("encode");
    assert_eq!(
        crate::cooked::cooked_motion_database_file_version(&clip_file).expect("probe"),
        None
    );
    assert!(crate::cooked::migrate_cooked_motion_database_file(&clip_file).is_err());
}

/// A long stop: `seconds` of braking at 30 Hz database frames.
fn long_stop(seconds: f32) -> AnimationClip {
    let keys = (seconds * 60.0) as usize;
    let times: Vec<f32> = (0..=keys).map(|k| k as f32 / 60.0).collect();
    let z = |t: f32| 2.0 * t - t * t / seconds;
    let mut clip = AnimationClip::empty("long_stop", seconds);
    clip.tracks.push(JointTrack {
        joint: ROOT,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(0.1 * t, 0.0, z(t))).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip
}

/// Hand-run: the arc-length table's first use (cooked arrays against sampling the clip) and the
/// clip-speed lookup, interleaved A/B on one host so drift hits both sides equally.
#[test]
#[ignore = "hand-run timing; run in release with --ignored --nocapture"]
fn perf_micro_cooked_arc_lengths_and_clip_speed() {
    use std::time::Instant;

    let clip = long_stop(10.0);
    let clips = vec![("anim/stop.clip".to_string(), clip.clone())];
    let text = r#"( format_version: 2, skeleton: "s", schema: ( root_joint: "root", feet: ["foot_l", "foot_r"] ),
                    entries: [ ( clip: "anim/stop.clip", distance_matched: true ) ] )"#;
    let db = MotionDatabaseSource::from_ron_str(text)
        .expect("parse")
        .build(&skeleton(), lookup(&clips))
        .expect("build");
    let with = MotionDatabase::from_cooked_payload(&db.to_cooked_payload().expect("c")).expect("v3");
    let without = MotionDatabase::from_cooked_payload(&db.to_cooked_payload_v2().expect("c")).expect("v2");
    let skeleton = Arc::new(skeleton());
    let shared: Arc<[AnimationClip]> = Arc::from(vec![clip]);
    let table_us = |database: &MotionDatabase| {
        let mut player = MotionMatchPlayer::new(shared.clone(), skeleton.clone());
        let started = Instant::now();
        let total = player.arc_table(database, 0).map(|t| t.total());
        let us = started.elapsed().as_secs_f64() * 1e6;
        assert!(total.is_some());
        us
    };
    // The clip speed: the stored array against the expression the player used before
    // (`sqrt(vx·vx + vz·vz)` of the raw root velocity).
    let speed_ns = |stored: bool| {
        let started = Instant::now();
        let mut sum = 0.0f32;
        for _ in 0..100 {
            for frame in 0..with.len() {
                sum += if stored {
                    with.clip_speed(frame).unwrap_or(0.0)
                } else {
                    with.raw_features(frame)
                        .map_or(0.0, |raw| (raw[0] * raw[0] + raw[2] * raw[2]).sqrt())
                };
            }
        }
        let ns = started.elapsed().as_secs_f64() * 1e9 / (100 * with.len()) as f64;
        assert!(std::hint::black_box(sum).is_finite());
        ns
    };
    let (mut a, mut b, mut c, mut d) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for round in 0..201 {
        // Alternate which side goes first.
        if round % 2 == 0 {
            a.push(table_us(&with));
            b.push(table_us(&without));
            c.push(speed_ns(true));
            d.push(speed_ns(false));
        } else {
            b.push(table_us(&without));
            a.push(table_us(&with));
            d.push(speed_ns(false));
            c.push(speed_ns(true));
        }
    }
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (a, b, c, d) = (median(a), median(b), median(c), median(d));
    println!(
        "perf-micro spec52_arc_table_first_use: frames={} cooked_us={a:.2} sampled_us={b:.2} speedup={:.1}x",
        with.len(),
        b / a
    );
    println!(
        "perf-micro spec52_clip_speed_lookup: stored_ns={c:.2} computed_ns={d:.2} ratio={:.2}",
        d / c
    );
}
