// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::sequencer::asset::{Ease, Marker, Track};
use crate::sequencer::marker_sheet::markers_to_csv;

fn float_track(name: &str, keys: &[(f32, f32)]) -> Track {
    Track {
        name: name.into(),
        target: "ui".into(),
        property: name.into(),
        keys: Channel::Float(
            keys.iter()
                .map(|&(t, v)| ScalarKey::new(t, v, Ease::Bezier))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn vec3_track(name: &str, keys: &[(f32, [f32; 3])]) -> Track {
    Track {
        name: name.into(),
        target: "cube".into(),
        property: "transform.position".into(),
        keys: Channel::Vec3(
            keys.iter()
                .map(|&(t, v)| Vec3Key::new(t, v, Ease::Bezier))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn bool_track(name: &str, keys: &[(f32, bool)]) -> Track {
    Track {
        name: name.into(),
        target: "lamp".into(),
        property: "visible".into(),
        keys: Channel::Bool(
            keys.iter()
                .map(|&(time, value)| BoolKey { time, value })
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn three_tracks() -> Sequence {
    Sequence {
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 2.0), (2.0, 1.0)]),
            vec3_track(
                "b",
                &[(0.0, [0.0; 3]), (1.5, [1.0, 2.0, 3.0]), (3.0, [0.0, 4.0, 0.0])],
            ),
            bool_track("c", &[(0.0, false), (1.0, true)]),
        ],
        ..Sequence::default()
    }
}

fn all_keys(sequence: &Sequence) -> Vec<KeyRef> {
    (0..sequence.tracks.len())
        .flat_map(|track| (0..sequence.tracks[track].keys.len()).map(move |index| KeyRef { track, index }))
        .collect()
}

fn sample(sequence: &Sequence, track: usize, time: f32) -> Vec<f32> {
    match sequence.tracks[track].keys.sample(time) {
        crate::sequencer::ChannelValue::Float(v) => vec![v],
        crate::sequencer::ChannelValue::Vec3(v) => v.to_vec(),
        crate::sequencer::ChannelValue::Bool(v) => vec![f32::from(u8::from(v))],
    }
}

/// Acceptance: keys box-selected on three tracks move +0.5 s; every key moves and the sampler
/// agrees (each track's curve is the old one shifted by 0.5 s).
#[test]
fn a_batch_move_shifts_every_selected_key_on_three_tracks_and_the_sampler_agrees() {
    let before = three_tracks();
    let mut after = before.clone();
    let keys = all_keys(&before);
    let selected = apply_key_batch(
        &mut after,
        &KeyBatch::Move {
            keys: keys.clone(),
            delta: 0.5,
        },
    )
    .expect("moved");
    assert_eq!(selected, keys, "the same keys stay selected");
    for key in &keys {
        let old = key_time(&before, *key).expect("old");
        assert_eq!(key_time(&after, *key), Some(old + 0.5), "{key:?}");
    }
    for track in 0..3 {
        for step in 0..=16 {
            let t = step as f32 * 0.25;
            assert_eq!(
                sample(&after, track, t + 0.5),
                sample(&before, track, t),
                "track {track} at {t}"
            );
        }
    }
}

/// Acceptance: scale x2 about the playhead; slopes follow, so a whole scaled curve is the old
/// curve stretched in time.
#[test]
fn a_batch_scale_about_the_playhead_stretches_the_curves() {
    let before = three_tracks();
    let mut after = before.clone();
    let pivot = 1.0;
    let err = apply_key_batch(
        &mut after,
        &KeyBatch::Scale {
            keys: all_keys(&before),
            pivot,
            factor: 2.0,
        },
    )
    .expect_err("a key at 0 s would land at -1 s");
    assert_eq!(err.code, "seq_keys_out_of_range");
    assert_eq!(after, before, "the key at 0 s cannot go to -1 s");
    let keys: Vec<KeyRef> = all_keys(&before)
        .into_iter()
        .filter(|k| key_time(&before, *k) > Some(0.0))
        .collect();
    let selected = apply_key_batch(
        &mut after,
        &KeyBatch::Scale {
            keys: keys.clone(),
            pivot,
            factor: 2.0,
        },
    )
    .expect("scaled");
    assert_eq!(selected.len(), keys.len());
    for key in &keys {
        let old = key_time(&before, *key).expect("old");
        assert_eq!(
            key_time(&after, *key),
            Some(pivot + (old - pivot) * 2.0),
            "{key:?}"
        );
    }
    // Explicit slopes are halved.
    let mut aligned = Sequence {
        tracks: vec![float_track("s", &[(1.0, 0.0), (2.0, 1.0)])],
        ..Sequence::default()
    };
    if let Channel::Float(k) = &mut aligned.tracks[0].keys {
        k[0].tangent = Tangent::Aligned(4.0);
    }
    let stretched_from = aligned.clone();
    apply_key_batch(
        &mut aligned,
        &KeyBatch::Scale {
            keys: vec![KeyRef { track: 0, index: 0 }, KeyRef { track: 0, index: 1 }],
            pivot: 1.0,
            factor: 2.0,
        },
    )
    .expect("scaled");
    assert_eq!(
        aligned.tracks[0].keys.tangent_slopes(0, 0).map(|s| s.1),
        Some(2.0)
    );
    for step in 0..=8 {
        let t = 1.0 + step as f32 * 0.125;
        let old = stretched_from.tracks[0].keys.sample_component(t, 0).expect("old");
        let new = aligned.tracks[0]
            .keys
            .sample_component(1.0 + (t - 1.0) * 2.0, 0)
            .expect("new");
        assert!((old - new).abs() < 1e-5, "at {t}: {old} vs {new}");
    }
}

/// Acceptance: keys copied from one sequence paste at the playhead of another, by track name or
/// by binding and property.
#[test]
fn copied_keys_paste_into_a_second_sequence() {
    let source = three_tracks();
    let keys = vec![
        KeyRef { track: 0, index: 1 },
        KeyRef { track: 0, index: 2 },
        KeyRef { track: 1, index: 1 },
    ];
    let clip = copy_keys(&source, &keys).expect("copied");
    assert_eq!(clip.len(), 3);
    // The second sequence names the vec3 track differently but binds the same property.
    let mut second = Sequence {
        tracks: vec![
            vec3_track("cube.position", &[(0.0, [9.0; 3])]),
            float_track("a", &[(0.0, 5.0), (4.0, 5.0)]),
        ],
        ..Sequence::default()
    };
    let selected = apply_key_batch(
        &mut second,
        &KeyBatch::Paste {
            clip: clip.clone(),
            at: 2.0,
        },
    )
    .expect("pasted");
    assert_eq!(selected.len(), 3);
    // Track `a` of the source: keys at 1 s and 2 s (relative 0 and 1 from the earliest key at 1 s).
    assert_eq!(key_times(&second, 1), Some(vec![0.0, 2.0, 3.0, 4.0]));
    assert_eq!(second.tracks[1].keys.sample_component(2.0, 0), Some(2.0));
    assert_eq!(second.tracks[1].keys.sample_component(3.0, 0), Some(1.0));
    // The vec3 key at 1.5 s lands at 2.5 s on `cube.position`.
    assert_eq!(key_times(&second, 0), Some(vec![0.0, 2.5]));
    assert_eq!(
        second.tracks[0].keys.sample(2.5),
        crate::sequencer::ChannelValue::Vec3([1.0, 2.0, 3.0])
    );
    // A paste over existing keys replaces them (same times), so pasting twice changes nothing more.
    let once = second.clone();
    apply_key_batch(&mut second, &KeyBatch::Paste { clip, at: 2.0 }).expect("pasted again");
    assert_eq!(second, once);
    // A sequence without a matching track refuses, unchanged.
    let mut empty = Sequence {
        tracks: vec![bool_track("x", &[(0.0, true)])],
        ..Sequence::default()
    };
    let keep = empty.clone();
    let clip = copy_keys(&source, &keys).expect("copied");
    let err = apply_key_batch(&mut empty, &KeyBatch::Paste { clip, at: 0.0 }).expect_err("no track");
    assert_eq!(err.code, "seq_keys_no_track");
    assert_eq!(empty, keep);
}

#[test]
fn refused_batches_change_nothing_and_name_a_code() {
    let start = three_tracks();
    let mut s = start.clone();
    let code = |s: &mut Sequence, batch: KeyBatch| apply_key_batch(s, &batch).expect_err("refused").code;
    let k = |track, index| KeyRef { track, index };
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Move {
                keys: vec![],
                delta: 1.0
            }
        ),
        "seq_keys_empty"
    );
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Move {
                keys: vec![k(9, 0)],
                delta: 1.0
            }
        ),
        "seq_keys_bad_track"
    );
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Move {
                keys: vec![k(0, 9)],
                delta: 1.0
            }
        ),
        "seq_keys_bad_key"
    );
    // Key 0 of `a` onto key 1 (1 s) is a collision with an unselected key.
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Move {
                keys: vec![k(0, 0)],
                delta: 1.0
            }
        ),
        "seq_keys_time_taken"
    );
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Move {
                keys: vec![k(0, 0)],
                delta: -0.1
            }
        ),
        "seq_keys_out_of_range"
    );
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Move {
                keys: vec![k(0, 1)],
                delta: f32::NAN
            }
        ),
        "seq_keys_out_of_range"
    );
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Scale {
                keys: vec![k(0, 1)],
                pivot: 0.0,
                factor: 0.0
            }
        ),
        "seq_keys_bad_scale"
    );
    assert_eq!(
        code(
            &mut s,
            KeyBatch::Delete {
                keys: vec![k(2, 0), k(2, 1)]
            }
        ),
        "seq_keys_last_key"
    );
    assert_eq!(s, start, "refused batches change nothing");
    // Moving a key past its neighbor re-sorts; the selection follows it.
    let selected = apply_key_batch(
        &mut s,
        &KeyBatch::Move {
            keys: vec![k(0, 0)],
            delta: 1.5,
        },
    )
    .expect("moved");
    assert_eq!(selected, vec![k(0, 1)]);
    assert_eq!(key_times(&s, 0), Some(vec![1.0, 1.5, 2.0]));
    // A delete across tracks.
    apply_key_batch(
        &mut s,
        &KeyBatch::Delete {
            keys: vec![k(0, 0), k(1, 2), k(2, 1)],
        },
    )
    .expect("deleted");
    assert_eq!(key_times(&s, 0), Some(vec![1.5, 2.0]));
    assert_eq!(key_times(&s, 1), Some(vec![0.0, 1.5]));
    assert_eq!(key_times(&s, 2), Some(vec![0.0]));
    // Section tracks have no keys.
    let mut with_clip = start.clone();
    with_clip.tracks.push(Track {
        name: "anim".into(),
        target: "rig".into(),
        property: String::new(),
        keys: Channel::Clip(vec![crate::sequencer::ClipSection {
            clip: "walk".into(),
            start: 0.0,
            end: 1.0,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        }]),
        enabled: true,
        locked: false,
    });
    assert_eq!(
        code(&mut with_clip, KeyBatch::Delete { keys: vec![k(3, 0)] }),
        "seq_keys_not_keyed"
    );
}

#[test]
fn snapping_rounds_to_the_nearest_frame() {
    assert_eq!(snap_to_frame(0.51, 24.0), 0.5);
    assert_eq!(snap_to_frame(1.0 / 30.0 + 0.001, 30.0), 1.0 / 30.0);
    assert_eq!(snap_to_frame(0.37, 0.0), 0.37);
}

/// Acceptance: marker add, move, rename and delete round-trip through save and reload, and a
/// CSV export then import is byte-equal.
#[test]
fn marker_edits_round_trip_through_save_and_reload_and_csv() {
    use crate::sequencer::edit::tracks::{MarkerEdit, apply_marker_edit};
    let mut s = three_tracks();
    let marker = |name: &str, time: f32| Marker {
        name: name.into(),
        time,
        payload: String::new(),
    };
    apply_marker_edit(
        &mut s,
        MarkerEdit::Add {
            at: None,
            marker: marker("door", 1.0),
        },
    )
    .expect("add");
    apply_marker_edit(
        &mut s,
        MarkerEdit::Add {
            at: None,
            marker: marker("gone", 2.0),
        },
    )
    .expect("add");
    set_marker(&mut s, 0, Some(1.25), None).expect("move");
    set_marker(&mut s, 0, None, Some("play, \"creak\"")).expect("payload");
    apply_marker_edit(
        &mut s,
        MarkerEdit::Rename {
            index: 0,
            name: "door_open".into(),
        },
    )
    .expect("rename");
    apply_marker_edit(&mut s, MarkerEdit::Remove { index: 1 }).expect("delete");
    let text = s.to_ron().expect("save");
    let back = Sequence::from_ron(&text).expect("reload");
    assert_eq!(
        back.markers,
        vec![Marker {
            name: "door_open".into(),
            time: 1.25,
            payload: "play, \"creak\"".into(),
        }]
    );
    assert_eq!(back.to_ron().expect("resave"), text, "byte-identical save");
    // Refusals change nothing.
    let keep = s.clone();
    assert_eq!(
        set_marker(&mut s, 5, Some(1.0), None).expect_err("bad").code,
        "seq_marker_bad_index"
    );
    assert_eq!(
        set_marker(&mut s, 0, Some(-1.0), None).expect_err("neg").code,
        "seq_marker_invalid"
    );
    assert_eq!(
        import_marker_csv(&mut s, "nope\n").expect_err("csv").code,
        "seq_marker_csv"
    );
    assert_eq!(s, keep);
    // CSV export, import into another sequence, export again: byte-equal.
    let csv = markers_to_csv(&s.markers);
    let mut other = Sequence::default();
    assert_eq!(import_marker_csv(&mut other, &csv), Ok(1));
    assert_eq!(markers_to_csv(&other.markers), csv);
    assert_eq!(other.markers, s.markers);
}

fn pose(x: f32) -> CutPose {
    CutPose {
        position: [x, 1.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        fov_degrees: 50.0,
    }
}

fn moving_shot(name: &str, duration: f32) -> Shot {
    let mut shot = still_shot(name, pose(0.0), duration);
    shot.camera.path.keys = vec![
        PathKey {
            time: 0.0,
            position: [0.0, 0.0, 0.0],
        },
        PathKey {
            time: duration,
            position: [10.0, 0.0, 0.0],
        },
    ];
    shot.camera.rotation.clear();
    shot.camera.path.look_at = LookAt::Point([0.0, 0.0, -10.0]);
    shot.camera.fov = ShotTrack(vec![
        crate::shots::ScalarKey {
            time: 0.0,
            value: 40.0,
            ease: crate::shots::Ease::Linear,
        },
        crate::shots::ScalarKey {
            time: duration,
            value: 80.0,
            ease: crate::shots::Ease::Linear,
        },
    ]);
    shot
}

fn list(shots: Vec<Shot>) -> ShotSequence {
    ShotSequence {
        shots,
        ..ShotSequence::default()
    }
}

#[test]
fn a_cut_splits_the_shot_and_keeps_the_first_part_as_it_played() {
    let before = list(vec![moving_shot("a", 4.0)]);
    let mut shots = before.clone();
    assert_eq!(
        apply_cut_edit(
            &mut shots,
            &CutEdit::Add {
                time: 1.0,
                pose: pose(7.0),
                ripple: false,
            }
        ),
        Ok(1)
    );
    assert_eq!(cut_times(&shots), vec![1.0]);
    assert_eq!(shots.duration(), 4.0, "the list keeps its length");
    for step in 0..=9 {
        let t = f64::from(step) * 0.1;
        let (a, b) = (before.sample(t).expect("a"), shots.sample(t).expect("b"));
        assert!(
            (a.eye - b.eye).length() < 1e-5 && (a.fov_y_degrees - b.fov_y_degrees).abs() < 1e-5,
            "at {t}"
        );
    }
    let after = shots.sample(1.0).expect("cut");
    assert_eq!(
        after.eye.to_array(),
        [7.0, 1.0, 0.0],
        "the new shot's camera from the cut"
    );
    assert_eq!(after.eye, shots.sample(3.9).expect("still").eye, "a still shot");
    // Move, trim, fade, delete; refusals change nothing.
    apply_cut_edit(
        &mut shots,
        &CutEdit::Move {
            cut: 0,
            time: 2.0,
            ripple: false,
        },
    )
    .expect("move");
    assert_eq!(cut_times(&shots), vec![2.0]);
    assert_eq!(shots.duration(), 4.0);
    apply_cut_edit(
        &mut shots,
        &CutEdit::Trim {
            shot: 1,
            duration: 1.0,
            ripple: false,
        },
    )
    .expect("trim");
    assert_eq!(shots.duration(), 3.0);
    apply_cut_edit(
        &mut shots,
        &CutEdit::Fade {
            shot: 1,
            fade_in: Some(0.25),
            fade_out: Some(0.5),
        },
    )
    .expect("fade");
    let curve = fade_curve(&shots, 13);
    assert_eq!(curve.first().map(|p| p.1), Some(0.0));
    assert_eq!(curve.last().map(|p| p.1), Some(1.0), "faded out at the end");
    let keep = shots.clone();
    let code = |s: &mut ShotSequence, e: CutEdit| apply_cut_edit(s, &e).expect_err("refused").code;
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Add {
                time: 2.0,
                pose: pose(0.0),
                ripple: false,
            }
        ),
        "seq_cut_time_taken"
    );
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Add {
                time: 3.0,
                pose: pose(0.0),
                ripple: false,
            }
        ),
        "seq_cut_out_of_range"
    );
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Move {
                cut: 0,
                time: 3.5,
                ripple: false,
            },
        ),
        "seq_cut_out_of_range"
    );
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Move {
                cut: 1,
                time: 1.0,
                ripple: false,
            },
        ),
        "seq_cut_bad_index"
    );
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Trim {
                shot: 0,
                duration: 0.0,
                ripple: false,
            }
        ),
        "seq_cut_out_of_range"
    );
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Fade {
                shot: 1,
                fade_in: Some(0.9),
                fade_out: None
            }
        ),
        "seq_cut_invalid"
    );
    let mut wide = pose(0.0);
    wide.fov_degrees = 200.0;
    assert_eq!(
        code(
            &mut shots,
            CutEdit::Add {
                time: 0.5,
                pose: wide,
                ripple: false,
            }
        ),
        "seq_cut_invalid"
    );
    assert_eq!(shots, keep, "refused edits change nothing");
    apply_cut_edit(&mut shots, &CutEdit::Delete { cut: 0 }).expect("delete");
    assert_eq!(shots.shots.len(), 1);
    assert_eq!(shots.duration(), 3.0);
    assert!(
        new_shot_list(pose(0.0), 0.0, 24.0).is_err(),
        "a shot needs a length"
    );
    assert_eq!(new_shot_list(pose(0.0), 2.0, 24.0).map(|l| l.duration()), Ok(2.0));
}

/// Acceptance: a cut added at 1.0 s switches the takeover camera at 1.0 s in a headless play.
#[cfg(feature = "client")]
#[test]
fn a_cut_added_at_one_second_switches_the_takeover_camera_at_one_second() {
    use std::time::Duration;

    use regia_app::prelude::*;
    use regia_core::math::Vec3;
    use regia_render::Camera;
    use regia_scene::Transform;

    use crate::camera::CameraPlugin;
    use crate::sequencer::{SequencePlayer, SequencerPlugin};

    let mut shots = list(vec![still_shot("a", pose(3.0), 3.0)]);
    apply_cut_edit(
        &mut shots,
        &CutEdit::Add {
            time: 1.0,
            pose: pose(-3.0),
            ripple: false,
        },
    )
    .expect("cut");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CameraPlugin, SequencerPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(125)));
    let camera = app
        .world_mut()
        .spawn((
            Transform {
                translation: Vec3::new(0.0, 0.0, 10.0),
                ..Transform::default()
            },
            Camera::default(),
        ))
        .id();
    let sequence = Sequence {
        tracks: vec![float_track("t", &[(0.0, 0.0), (3.0, 1.0)])],
        ..Sequence::default()
    };
    let mut player = SequencePlayer::new(sequence).with_shots(shots);
    player.takeover.blend_in = 0.0;
    player.takeover.blend_out = 0.0;
    player.play();
    let player = app.world_mut().spawn(player).id();
    let mut switched_at = None;
    for _ in 0..40 {
        app.update();
        let time = app.world().get::<SequencePlayer>(player).expect("player").time();
        let x = app
            .world()
            .get::<Transform>(camera)
            .expect("camera")
            .translation
            .x;
        if time < 1.0 {
            assert_eq!(x, 3.0, "the first shot before 1 s (clock {time})");
        } else if time < 3.0 {
            assert_eq!(x, -3.0, "the cut's camera from 1 s (clock {time})");
            switched_at.get_or_insert(time);
        }
    }
    assert_eq!(
        switched_at,
        Some(1.0),
        "the first frame at the cut shows the new camera"
    );
}

/// A sequence and a shot list timed to match: three shots of 2 s (cuts at 2 and 4), keys and
/// markers around the ripple point.
fn ripple_fixture() -> (Sequence, ShotSequence) {
    let sequence = Sequence {
        tracks: vec![float_track(
            "a",
            &[(0.5, 0.0), (1.0, 2.0), (4.5, 1.0), (5.5, 3.0)],
        )],
        markers: vec![
            Marker {
                name: "stays".into(),
                time: 3.4,
                payload: String::new(),
            },
            Marker {
                name: "early".into(),
                time: 3.7,
                payload: String::new(),
            },
            Marker {
                name: "late".into(),
                time: 5.0,
                payload: String::new(),
            },
        ],
        ..Sequence::default()
    };
    let shots = list(vec![
        moving_shot("one", 2.0),
        moving_shot("two", 2.0),
        moving_shot("three", 2.0),
    ]);
    (sequence, shots)
}

/// Acceptance (ripple edit): trimming shot 1 by -0.5 s with ripple moves every key, marker and
/// cut after the shot's old end exactly 0.5 s earlier and the sampler agrees; without ripple the
/// keys and markers do not move. The ripple point is the old edge (w9 review finding 3): a key or
/// marker in the span the trim removes (3.5..=4 s) refuses the ripple rather than reorder, and a
/// refused ripple changes nothing at all.
#[test]
fn a_ripple_trim_moves_every_later_key_marker_and_cut() {
    let (sequence, shots) = ripple_fixture();
    let edit = CutEdit::Trim {
        shot: 1,
        duration: 1.5,
        ripple: true,
    };
    // The span the caller shifts the sequence by, read before anything changes: the old end
    // (4 s) and the delta.
    assert_eq!(ripple_span(&shots, &edit), Some((4.0, -0.5)));
    // The fixture's "early" marker (3.7 s) sits in the removed span: refused, nothing moves.
    let mut refused = sequence.clone();
    assert_eq!(
        ripple_shift(&mut refused, 4.0, -0.5)
            .expect_err("early is in the span")
            .code,
        "seq_ripple_keys_in_span"
    );
    assert_eq!(refused, sequence, "a refused ripple changes nothing");
    // Without it, the order-preserving result: everything after 4 s moves 0.5 s earlier.
    let mut sequence = sequence;
    sequence.markers.retain(|m| m.name != "early");
    let mut cut_shots = shots.clone();
    let mut cut_sequence = sequence.clone();
    apply_cut_edit(&mut cut_shots, &edit).expect("trim");
    ripple_shift(&mut cut_sequence, 4.0, -0.5).expect("ripple");
    // The later cut is exactly 0.5 s earlier; the first cut and the untouched keys stay.
    assert_eq!(cut_times(&cut_shots), vec![2.0, 3.5]);
    assert_eq!(cut_shots.duration(), 5.5);
    assert_eq!(key_times(&cut_sequence, 0), Some(vec![0.5, 1.0, 4.0, 5.0]));
    assert_eq!(
        cut_sequence
            .markers
            .iter()
            .map(|m| (m.name.as_str(), m.time))
            .collect::<Vec<_>>(),
        vec![("stays", 3.4), ("late", 4.5)],
    );
    // The sampler agrees at three probe times inside the shifted span.
    for t in [4.1, 4.5, 4.9] {
        assert_eq!(
            cut_sequence.tracks[0].keys.sample_component(t, 0),
            sequence.tracks[0].keys.sample_component(t + 0.5, 0),
            "probe at {t}"
        );
    }
    // Without ripple (today's behavior): the later cut still moves, the keys and markers do not.
    let mut plain_shots = shots.clone();
    apply_cut_edit(
        &mut plain_shots,
        &CutEdit::Trim {
            shot: 1,
            duration: 1.5,
            ripple: false,
        },
    )
    .expect("trim");
    assert_eq!(cut_times(&plain_shots), vec![2.0, 3.5]);
    let mut plain_sequence = sequence.clone();
    ripple_shift(&mut plain_sequence, 4.0, 0.0).expect("zero shift moves nothing");
    assert_eq!(plain_sequence, sequence);
    // A key exactly at the old end is in the removed span too; a key exactly at the new end
    // stays and a later one never lands on it.
    let tight = Sequence {
        tracks: vec![float_track("t", &[(3.5, 0.0), (4.0, 1.0)])],
        ..Sequence::default()
    };
    let mut refused = tight.clone();
    assert_eq!(
        ripple_shift(&mut refused, 4.0, -0.5)
            .expect_err("4 s is in the span")
            .code,
        "seq_ripple_keys_in_span"
    );
    assert_eq!(refused, tight, "a refused ripple changes nothing");
    let mut kept = Sequence {
        tracks: vec![float_track("t", &[(3.5, 0.0), (4.01, 1.0)])],
        ..Sequence::default()
    };
    ripple_shift(&mut kept, 4.0, -0.5).expect("only keys after the old end move");
    assert_eq!(key_times(&kept, 0).map(|t| t[0]), Some(3.5));
    assert!(key_times(&kept, 0).is_some_and(|t| t[1] > 3.5), "order kept");
    assert_eq!(
        ripple_shift(&mut refused, f32::NAN, -0.5).expect_err("nan").code,
        "seq_keys_out_of_range"
    );
}

/// w10 item 2 (w9 review finding 3): a growing ripple shifts the keys and markers just after the
/// shot's old end, so they keep their place after the cut instead of landing inside the longer
/// shot; nothing before the old end moves.
#[test]
fn a_ripple_growth_shifts_the_keys_just_after_the_old_end() {
    let (mut sequence, shots) = ripple_fixture();
    sequence.markers.push(Marker {
        name: "after_cut".into(),
        time: 4.2,
        payload: String::new(),
    });
    // Shot 1 (2..4 s) grows to 2.5 s: the old end is 4 s, the delta +0.5 s.
    let edit = CutEdit::Trim {
        shot: 1,
        duration: 2.5,
        ripple: true,
    };
    let (point, delta) = ripple_span(&shots, &edit).expect("a ripple");
    assert_eq!((point, delta), (4.0, 0.5));
    ripple_shift(&mut sequence, point, delta).expect("ripple");
    assert_eq!(
        key_times(&sequence, 0),
        Some(vec![0.5, 1.0, 5.0, 6.0]),
        "the 4.5 s key, just after the old end, moves too"
    );
    let markers: Vec<(&str, f32)> = sequence
        .markers
        .iter()
        .map(|m| (m.name.as_str(), m.time))
        .collect();
    assert_eq!(
        markers,
        vec![("stays", 3.4), ("early", 3.7), ("late", 5.5), ("after_cut", 4.7)],
    );
}

/// w10 item 9 (w9 review finding 7): a rippling cut move keeps the second shot's duration, so
/// it may go past that shot's original end (the later cuts move with it); without ripple that
/// end still bounds the move.
#[test]
fn a_ripple_cut_move_may_pass_the_second_shots_original_end() {
    let (_, shots) = ripple_fixture();
    let mut rippled = shots.clone();
    apply_cut_edit(
        &mut rippled,
        &CutEdit::Move {
            cut: 0,
            time: 4.5,
            ripple: true,
        },
    )
    .expect("a rippling move past 4 s");
    assert_eq!(cut_times(&rippled), vec![4.5, 6.5]);
    assert_eq!(rippled.duration(), 8.5);
    let mut plain = shots.clone();
    assert_eq!(
        apply_cut_edit(
            &mut plain,
            &CutEdit::Move {
                cut: 0,
                time: 4.5,
                ripple: false,
            },
        )
        .expect_err("past the second shot's end")
        .code,
        "seq_cut_out_of_range"
    );
    assert_eq!(plain, shots);
}

/// Acceptance (ripple edit): moving a cut with ripple keeps the second shot's clock, so every
/// later cut moves by the same delta; without it the second shot absorbs the change and the later
/// cuts stay put.
#[test]
fn a_ripple_move_shifts_the_later_cuts_and_keys() {
    let (sequence, shots) = ripple_fixture();
    let edit = CutEdit::Move {
        cut: 0,
        time: 2.5,
        ripple: true,
    };
    assert_eq!(
        ripple_span(&shots, &edit),
        Some((2.0, 0.5)),
        "the old cut time and the delta"
    );
    let mut rippled = shots.clone();
    apply_cut_edit(&mut rippled, &edit).expect("move");
    assert_eq!(cut_times(&rippled), vec![2.5, 4.5], "both cuts move +0.5 s");
    assert_eq!(rippled.duration(), 6.5, "shot two keeps its 2 s");
    let mut plain = shots.clone();
    apply_cut_edit(
        &mut plain,
        &CutEdit::Move {
            cut: 0,
            time: 2.5,
            ripple: false,
        },
    )
    .expect("move");
    assert_eq!(cut_times(&plain), vec![2.5, 4.0], "the later cut stays put");
    assert_eq!(plain.duration(), 6.0);
    // The sequence side shifts keys and markers after the moved cut.
    let mut cut_sequence = sequence.clone();
    ripple_shift(&mut cut_sequence, 2.0, 0.5).expect("ripple");
    assert_eq!(key_times(&cut_sequence, 0), Some(vec![0.5, 1.0, 5.0, 6.0]));
    // A cut add never ripples: the split keeps the list's length.
    assert_eq!(
        ripple_span(
            &shots,
            &CutEdit::Add {
                time: 1.0,
                pose: pose(0.0),
                ripple: true,
            }
        ),
        None
    );
}

/// A sequence with one keyed track and one clip section track for the dope-sheet marks.
fn marks_fixture() -> Sequence {
    Sequence {
        tracks: vec![
            float_track("a", &[(0.5, 0.0), (2.0, 1.0)]),
            Track {
                name: "anim".into(),
                target: "rig".into(),
                property: String::new(),
                keys: Channel::Clip(vec![
                    crate::sequencer::ClipSection {
                        clip: "walk".into(),
                        start: 1.0,
                        end: 3.0,
                        offset: 0.0,
                        speed: 1.0,
                        looping: false,
                        blend_in: 0.0,
                        blend_out: 0.0,
                        weight: 1.0,
                    },
                    crate::sequencer::ClipSection {
                        clip: "run".into(),
                        start: 4.0,
                        end: 6.0,
                        offset: 0.0,
                        speed: 1.0,
                        looping: false,
                        blend_in: 0.0,
                        blend_out: 0.0,
                        weight: 1.0,
                    },
                ]),
                enabled: true,
                locked: false,
            },
        ],
        ..Sequence::default()
    }
}

/// Acceptance (section rows): a key and a section start selected together move +0.5 s as one
/// validated edit (the section start trims, the end stays); a whole-section move that would
/// overlap the next section is refused with a code and changes nothing.
#[test]
fn section_marks_move_with_keys_and_an_overlapping_move_is_refused() {
    let sequence = marks_fixture();
    // A box over 1.5..2.5 s picks the 2 s key and the walk start (1 s is outside; both marks of
    // the walk show up when the box covers them).
    let boxed = marks_in_box(&sequence, &[], 1.9, 2.1);
    assert_eq!(boxed, vec![MarkRef::Key(KeyRef { track: 0, index: 1 })]);
    let all = marks_in_box(&sequence, &[], 0.0, 10.0);
    assert_eq!(all.len(), 6, "two keys, two section starts, two ends");
    // Move the key and the section start together.
    let mut moved = sequence.clone();
    let selected = apply_mark_move(
        &mut moved,
        &[
            MarkRef::Key(KeyRef { track: 0, index: 1 }),
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: false,
            },
        ],
        0.5,
    )
    .expect("moved");
    assert_eq!(
        selected,
        vec![
            MarkRef::Key(KeyRef { track: 0, index: 1 }),
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: false
            },
        ]
    );
    assert_eq!(key_times(&moved, 0), Some(vec![0.5, 2.5]));
    let Channel::Clip(sections) = &moved.tracks[1].keys else {
        unreachable!()
    };
    assert_eq!(
        (sections[0].start, sections[0].end, sections[0].offset),
        (1.5, 3.0, 0.5)
    );
    assert_eq!(
        (sections[1].start, sections[1].end),
        (4.0, 6.0),
        "the other section stays"
    );
    // Both ends selected: the whole section moves, keeping its length and offset.
    let mut whole = sequence.clone();
    apply_mark_move(
        &mut whole,
        &[
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: false,
            },
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: true,
            },
        ],
        0.5,
    )
    .expect("whole move");
    let Channel::Clip(sections) = &whole.tracks[1].keys else {
        unreachable!()
    };
    assert_eq!(
        (sections[0].start, sections[0].end, sections[0].offset),
        (1.5, 3.5, 0.0)
    );
    // A move that would overlap two sections is refused with a code and changes nothing,
    // including the keys that were part of the same gesture.
    let keep = moved.clone();
    let err = apply_mark_move(
        &mut moved,
        &[
            MarkRef::Key(KeyRef { track: 0, index: 1 }),
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: false,
            },
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: true,
            },
        ],
        1.5,
    )
    .expect_err("overlap");
    assert_eq!(err.code, "seq_section_overlap");
    assert_eq!(moved, keep, "the refused move changed nothing");
    // Mixed kinds are refused: a key on a section track, a section mark on a keyed track.
    assert_eq!(
        apply_mark_move(&mut moved, &[MarkRef::Key(KeyRef { track: 1, index: 0 })], 0.5,)
            .expect_err("key on sections")
            .code,
        "seq_keys_not_keyed"
    );
    assert_eq!(
        apply_mark_move(
            &mut moved,
            &[MarkRef::SectionEnd {
                track: 0,
                index: 0,
                end: false
            }],
            0.5,
        )
        .expect_err("section on keys")
        .code,
        "seq_keys_not_keyed"
    );
    assert_eq!(moved, keep);
}

/// A 4-key Catmull-Rom camera shot with a keyed look-at track and Linear-eased rotations.
fn catmull_shot() -> Shot {
    let mut shot = still_shot("cam", pose(0.0), 3.0);
    shot.camera.path.interp = PathInterp::CatmullRom;
    shot.camera.path.keys = vec![
        PathKey {
            time: 0.0,
            position: [0.0, 0.0, 0.0],
        },
        PathKey {
            time: 1.0,
            position: [1.0, 2.0, 0.5],
        },
        PathKey {
            time: 2.0,
            position: [4.0, 3.0, 1.0],
        },
        PathKey {
            time: 3.0,
            position: [8.0, 0.0, 2.0],
        },
    ];
    shot.camera.path.look_at = LookAt::Track(vec![
        PathKey {
            time: 0.0,
            position: [0.0, 0.0, -5.0],
        },
        PathKey {
            time: 1.0,
            position: [1.0, 1.0, -4.0],
        },
        PathKey {
            time: 2.0,
            position: [4.0, 2.0, -3.0],
        },
        PathKey {
            time: 3.0,
            position: [8.0, 1.0, -2.0],
        },
    ]);
    shot.camera.rotation = vec![
        RotationKey {
            time: 0.0,
            rotation: [0.0, 0.0, 0.0, 1.0],
            ease: crate::shots::Ease::Linear,
        },
        RotationKey {
            time: 1.5,
            rotation: [0.0, 0.38268_34, 0.0, 0.92387_95],
            ease: crate::shots::Ease::Linear,
        },
        RotationKey {
            time: 3.0,
            rotation: [0.0, 0.707_106_77, 0.0, 0.707_106_77],
            ease: crate::shots::Ease::Linear,
        },
    ];
    shot
}

/// Acceptance (Catmull-Rom cut end): a 4-key Catmull-Rom camera shot cut at 60 % of its third
/// segment samples the uncut shot's positions, look-at targets and rotations at every one of 50
/// times before the cut, and the same holds after a further trim. (Failed on the base: the cut
/// dropped the last key, so the second segment's end tangent bent.)
#[test]
fn a_shortened_catmull_rom_shot_samples_its_uncut_curve_before_the_cut() {
    let before = list(vec![catmull_shot()]);
    let mut cut = before.clone();
    apply_cut_edit(
        &mut cut,
        &CutEdit::Add {
            time: 2.6,
            pose: pose(7.0),
            ripple: false,
        },
    )
    .expect("cut");
    let mut largest = 0.0f32;
    for i in 0..50 {
        let t = 2.6 * f64::from(i) / 50.0;
        let (a, b) = (before.sample(t).expect("before"), cut.sample(t).expect("cut"));
        let eye = (a.eye - b.eye).length();
        let target = (a.target - b.target).length();
        let turn = a.rotation.angle_between(b.rotation);
        largest = largest.max(eye);
        assert!(
            eye < 1e-5 && target < 1e-5 && turn < 1e-5,
            "at {t:.3} s: eye {eye:.6} m, target {target:.6} m, rotation {turn:.6} rad"
        );
    }
    eprintln!("catmull-rom cut end: largest eye error over 50 samples = {largest:.6} m");
    // The same after a trim of the already-cut shot, against the original uncut shot.
    let mut trimmed = cut.clone();
    apply_cut_edit(
        &mut trimmed,
        &CutEdit::Trim {
            shot: 0,
            duration: 2.2,
            ripple: false,
        },
    )
    .expect("trim");
    for i in 0..50 {
        let t = 2.2 * f64::from(i) / 50.0;
        let (a, b) = (
            before.sample(t).expect("before"),
            trimmed.sample(t).expect("trim"),
        );
        assert!(
            (a.eye - b.eye).length() < 1e-5 && (a.target - b.target).length() < 1e-5,
            "at {t:.3} s: eye {}, target {}",
            (a.eye - b.eye).length(),
            (a.target - b.target).length()
        );
    }
    // A moved cut restores the curve it had: the kept keys come back into play.
    let mut moved_back = trimmed.clone();
    apply_cut_edit(
        &mut moved_back,
        &CutEdit::Move {
            cut: 0,
            time: 2.5,
            ripple: false,
        },
    )
    .expect("move");
    for i in 0..50 {
        let t = 2.5 * f64::from(i) / 50.0;
        let (a, b) = (
            before.sample(t).expect("before"),
            moved_back.sample(t).expect("move"),
        );
        assert!(
            (a.eye - b.eye).length() < 1e-5,
            "at {t:.3} s: {}",
            (a.eye - b.eye).length()
        );
    }
}

/// w10 item 7 (w9 review finding 5): sections may overlap (cross-fades), so the dope sheet
/// refuses only an overlap the move creates. On a track whose walk and run sections cross-fade,
/// a move that shrinks or keeps that overlap goes through; a move onto a third section is refused.
#[test]
fn a_mark_move_on_a_cross_faded_track_refuses_only_a_new_overlap() {
    let mut sequence = marks_fixture();
    let Channel::Clip(sections) = &mut sequence.tracks[1].keys else {
        unreachable!()
    };
    // walk 1..3 and run 2.5..6 cross-fade over 2.5..3; idle sits apart at 7..8.
    sections[1].start = 2.5;
    let mut idle = sections[1].clone();
    idle.clip = "idle".into();
    idle.start = 7.0;
    idle.end = 8.0;
    sections.push(idle);
    // The run start trims later: the cross-fade shrinks (2.7..3).
    let mut shrink = sequence.clone();
    apply_mark_move(
        &mut shrink,
        &[MarkRef::SectionEnd {
            track: 1,
            index: 1,
            end: false,
        }],
        0.2,
    )
    .expect("a move that shrinks an existing overlap");
    // The whole walk moves +0.2 s: it still overlaps run, as it did.
    let mut kept = sequence.clone();
    apply_mark_move(
        &mut kept,
        &[
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: false,
            },
            MarkRef::SectionEnd {
                track: 1,
                index: 0,
                end: true,
            },
        ],
        0.2,
    )
    .expect("a move that keeps the existing overlap");
    // The idle section moved onto run is a new overlap: refused, nothing changes.
    let mut refused = sequence.clone();
    assert_eq!(
        apply_mark_move(
            &mut refused,
            &[
                MarkRef::SectionEnd {
                    track: 1,
                    index: 2,
                    end: false
                },
                MarkRef::SectionEnd {
                    track: 1,
                    index: 2,
                    end: true
                },
            ],
            -1.5,
        )
        .expect_err("a new overlap")
        .code,
        "seq_section_overlap"
    );
    assert_eq!(refused, sequence);
}

/// A flat audio track with one `chime` section per span.
fn audio_track(name: &str, spans: &[(f32, f32)]) -> Track {
    Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Audio(
            spans
                .iter()
                .map(|&(a, b)| crate::sequencer::AudioSection::new("chime", a, b))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

/// The section spans of track `track` as `(start, end)` pairs, in section order.
fn spans_of(sequence: &Sequence, track: usize) -> Vec<(f32, f32)> {
    crate::sequencer::sections::section_spans(&sequence.tracks[track].keys)
        .unwrap_or_default()
        .iter()
        .map(|s| (s.start, s.end))
        .collect()
}

/// w11 item 3 (after ADR-8640): a ripple moves the section spans that start after the edge with
/// the keys (whole, length and content kept); a section that starts before the edge stays, even
/// when it runs past it; a shortening ripple over a section start is refused like one over a key.
#[test]
fn a_ripple_moves_the_later_sections_with_the_keys() {
    let sequence = Sequence {
        tracks: vec![
            float_track("a", &[(0.5, 0.0), (4.5, 1.0)]),
            audio_track("music", &[(0.5, 1.5), (4.2, 5.2)]),
            audio_track("pad", &[(3.0, 5.0)]),
        ],
        ..Sequence::default()
    };
    let mut grown = sequence.clone();
    ripple_shift(&mut grown, 4.0, 1.0).expect("growth");
    assert_eq!(key_times(&grown, 0), Some(vec![0.5, 5.5]));
    assert_eq!(
        spans_of(&grown, 1),
        vec![(0.5, 1.5), (5.2, 6.2)],
        "the later section moves"
    );
    assert_eq!(
        spans_of(&grown, 2),
        vec![(3.0, 5.0)],
        "a section across the edge stays"
    );
    let mut shrunk = sequence.clone();
    ripple_shift(&mut shrunk, 4.0, -0.5).expect("shrink");
    let music = spans_of(&shrunk, 1);
    assert!(
        (music[1].0 - 3.7).abs() < 1.0e-5 && (music[1].1 - 4.7).abs() < 1.0e-5,
        "{music:?}"
    );
    // A section starting in the removed span (3.5..=4 s) refuses the ripple; nothing changes.
    let mut tight = sequence.clone();
    tight.tracks[1] = audio_track("music", &[(0.5, 1.5), (3.8, 4.5)]);
    let before = tight.clone();
    assert_eq!(
        ripple_shift(&mut tight, 4.0, -0.5).expect_err("in the span").code,
        "seq_ripple_keys_in_span"
    );
    assert_eq!(tight, before, "a refused ripple changes nothing");
}

/// w11 item 3: `apply_mark_scale` scales keys and section marks about the pivot on one validated
/// copy: a section with both marks stretches, one mark trims that end; a bad factor or a refused
/// result changes nothing.
#[test]
fn a_mark_scale_stretches_whole_sections_trims_single_ends_and_scales_keys() {
    let sequence = Sequence {
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 1.0), (3.0, 2.0)]),
            audio_track("music", &[(1.0, 2.0), (4.0, 5.0)]),
        ],
        ..Sequence::default()
    };
    let marks = [
        MarkRef::Key(KeyRef { track: 0, index: 1 }),
        MarkRef::SectionEnd {
            track: 1,
            index: 0,
            end: false,
        },
        MarkRef::SectionEnd {
            track: 1,
            index: 0,
            end: true,
        },
        MarkRef::SectionEnd {
            track: 1,
            index: 1,
            end: true,
        },
    ];
    let mut scaled = sequence.clone();
    let out = apply_mark_scale(&mut scaled, &marks, 0.0, 2.0).expect("scale");
    assert_eq!(key_times(&scaled, 0), Some(vec![0.0, 2.0, 3.0]));
    assert_eq!(spans_of(&scaled, 1), vec![(2.0, 4.0), (4.0, 10.0)]);
    assert_eq!(out.len(), 4, "{out:?}");
    let mut refused = sequence.clone();
    assert_eq!(
        apply_mark_scale(&mut refused, &marks, 0.0, 0.0)
            .expect_err("zero")
            .code,
        "seq_keys_bad_scale"
    );
    // A start scaled past its own end is refused whole.
    let start_only = [MarkRef::SectionEnd {
        track: 1,
        index: 1,
        end: false,
    }];
    assert!(apply_mark_scale(&mut refused, &start_only, 0.0, 2.0).is_err());
    assert_eq!(refused, sequence, "a refused scale changes nothing");
}

/// w11 item 3: `copy_marks` copies keys and whole sections relative to the earliest copied mark;
/// `paste_marks` puts them back at the paste time (sections appended whole), refuses a pasted
/// section over an existing one, and `KeyBatch::Paste` of the same clip reports only the keys.
#[test]
fn copied_sections_paste_whole_relative_to_the_earliest_mark() {
    let sequence = Sequence {
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 1.0), (3.0, 2.0)]),
            audio_track("music", &[(1.0, 2.0), (4.0, 5.0)]),
        ],
        ..Sequence::default()
    };
    let clip = copy_marks(
        &sequence,
        &[
            MarkRef::Key(KeyRef { track: 0, index: 2 }),
            MarkRef::SectionEnd {
                track: 1,
                index: 1,
                end: true,
            },
        ],
    )
    .expect("copy");
    assert_eq!(clip.len(), 2, "a key and a section");
    let mut pasted = sequence.clone();
    let marks = paste_marks(&mut pasted, &clip, 6.0).expect("paste");
    assert_eq!(key_times(&pasted, 0), Some(vec![0.0, 1.0, 3.0, 6.0]));
    assert_eq!(spans_of(&pasted, 1), vec![(1.0, 2.0), (4.0, 5.0), (7.0, 8.0)]);
    assert_eq!(
        marks,
        vec![
            MarkRef::Key(KeyRef { track: 0, index: 3 }),
            MarkRef::SectionEnd {
                track: 1,
                index: 2,
                end: false,
            },
            MarkRef::SectionEnd {
                track: 1,
                index: 2,
                end: true,
            },
        ]
    );
    // Over an existing section: refused, nothing changes.
    let mut refused = sequence.clone();
    assert_eq!(
        paste_marks(&mut refused, &clip, 3.5).expect_err("overlap").code,
        "seq_section_overlap"
    );
    assert_eq!(refused, sequence);
    // The key batch paste reports the keys only (the section pastes too).
    let mut batch = sequence.clone();
    let keys = apply_key_batch(&mut batch, &KeyBatch::Paste { clip, at: 6.0 }).expect("batch paste");
    assert_eq!(keys, vec![KeyRef { track: 0, index: 3 }]);
    assert_eq!(spans_of(&batch, 1).len(), 3);
}

fn event_track(name: &str, keys: &[(f32, &str)]) -> Track {
    Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Event(
            keys.iter()
                .map(|&(time, event)| crate::sequencer::asset::EventKey {
                    time,
                    event: event.into(),
                    payload: format!("{event}-payload"),
                })
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn events_of(sequence: &Sequence, track: usize) -> Vec<(f32, String)> {
    match &sequence.tracks[track].keys {
        Channel::Event(keys) => keys.iter().map(|k| (k.time, k.event.clone())).collect(),
        other => panic!("track {track} is a {} track", other.kind()),
    }
}

fn cue_sequence() -> Sequence {
    Sequence {
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 1.0)]),
            event_track("cues", &[(0.5, "door"), (1.0, "light"), (2.0, "end")]),
        ],
        ..Sequence::default()
    }
}

fn key(track: usize, index: usize) -> MarkRef {
    MarkRef::Key(KeyRef { track, index })
}

/// ADR-8628: event keys (sequence v6) copy and paste like the other keys. A pasted event goes
/// after the keys already at its time (events may share a time and fire in file order), keeps its
/// name and payload, and is reported as a key mark.
#[test]
fn event_keys_copy_and_paste_after_the_keys_at_their_time() {
    let sequence = cue_sequence();
    let clip = copy_marks(&sequence, &[key(1, 0), key(1, 1)]).expect("event keys copy");
    assert_eq!(clip.len(), 2);
    assert_eq!(clip.tracks.len(), 1);
    let Channel::Event(copied) = &clip.tracks[0].keys else {
        panic!("an Event clip: {:?}", clip.tracks[0].keys);
    };
    assert_eq!(
        copied
            .iter()
            .map(|k| (k.time, k.event.as_str()))
            .collect::<Vec<_>>(),
        [(0.0, "door"), (0.5, "light")],
        "times relative to the earliest copied key"
    );

    let mut pasted = sequence.clone();
    let marks = paste_marks(&mut pasted, &clip, 2.0).expect("paste");
    assert_eq!(
        events_of(&pasted, 1),
        [
            (0.5, "door".to_string()),
            (1.0, "light".to_string()),
            (2.0, "end".to_string()),
            (2.0, "door".to_string()),
            (2.5, "light".to_string()),
        ],
        "the pasted `door` goes after `end`, which was at 2 s first"
    );
    assert_eq!(marks, [key(1, 3), key(1, 4)], "the pasted keys are selected");
    let Channel::Event(keys) = &pasted.tracks[1].keys else {
        unreachable!()
    };
    assert_eq!(keys[3].payload, "door-payload", "the payload pastes with the key");
    assert_eq!(
        pasted.tracks[0], sequence.tracks[0],
        "the float track is untouched"
    );

    // The key batch paste takes the same path and reports the keys.
    let mut batch = sequence.clone();
    let selected = apply_key_batch(
        &mut batch,
        &KeyBatch::Paste {
            clip: clip.clone(),
            at: 2.0,
        },
    )
    .expect("batch paste");
    assert_eq!(batch, pasted);
    assert_eq!(
        selected,
        [KeyRef { track: 1, index: 3 }, KeyRef { track: 1, index: 4 }]
    );

    // A mixed clip: a float key and an event key paste together, each into its own track.
    let mixed = copy_marks(&sequence, &[key(0, 1), key(1, 2)]).expect("mixed copy");
    let mut both = sequence.clone();
    let marks = paste_marks(&mut both, &mixed, 3.0).expect("mixed paste");
    assert_eq!(key_times(&both, 0), Some(vec![0.0, 1.0, 3.0]));
    assert_eq!(events_of(&both, 1).last(), Some(&(4.0, "end".to_string())));
    assert_eq!(marks, [key(0, 2), key(1, 3)]);

    // A paste below zero is refused and changes nothing.
    let mut early = sequence.clone();
    assert_eq!(
        paste_marks(&mut early, &clip, -1.0).expect_err("below zero").code,
        "seq_keys_out_of_range"
    );
    assert_eq!(early, sequence);
}

/// ADR-8628: event keys box-select, move, scale and delete as keys; a move onto another event's
/// time is not refused (events may share a time) and keeps file order; a section mark on an
/// event track is refused as on any track with keys.
#[test]
fn event_keys_move_scale_delete_and_box_select_like_other_keys() {
    let sequence = cue_sequence();
    assert_eq!(
        marks_in_box(&sequence, &[1], 0.4, 1.1),
        [key(1, 0), key(1, 1)],
        "the box selects event keys"
    );
    assert_eq!(key_time(&sequence, KeyRef { track: 1, index: 2 }), Some(2.0));

    let mut moved = sequence.clone();
    let marks = apply_mark_move(&mut moved, &[key(1, 0)], 0.5).expect("onto `light`'s time");
    assert_eq!(
        events_of(&moved, 1),
        [
            (1.0, "door".to_string()),
            (1.0, "light".to_string()),
            (2.0, "end".to_string())
        ],
        "a stable order: `door` was first"
    );
    assert_eq!(marks, [key(1, 0)]);

    let mut scaled = sequence.clone();
    apply_mark_scale(&mut scaled, &[key(1, 1), key(1, 2)], 1.0, 2.0).expect("scale");
    assert_eq!(
        events_of(&scaled, 1).iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        [0.5, 1.0, 3.0]
    );

    let mut deleted = sequence.clone();
    apply_key_batch(
        &mut deleted,
        &KeyBatch::Delete {
            keys: vec![KeyRef { track: 1, index: 1 }],
        },
    )
    .expect("delete one");
    assert_eq!(
        events_of(&deleted, 1),
        [(0.5, "door".to_string()), (2.0, "end".to_string())]
    );
    let mut all = sequence.clone();
    let every: Vec<KeyRef> = (0..3).map(|index| KeyRef { track: 1, index }).collect();
    assert_eq!(
        apply_key_batch(&mut all, &KeyBatch::Delete { keys: every })
            .expect_err("last key")
            .code,
        "seq_keys_last_key"
    );
    assert_eq!(all, sequence);

    let mut wrong = sequence.clone();
    let end = MarkRef::SectionEnd {
        track: 1,
        index: 0,
        end: false,
    };
    assert_eq!(
        apply_mark_move(&mut wrong, &[end], 1.0)
            .expect_err("no sections")
            .code,
        "seq_keys_not_keyed"
    );
    assert_eq!(
        copy_marks(&sequence, &[end]).expect_err("no sections").code,
        "seq_section_bad_index"
    );
}

/// ADR-8628: a ripple shifts the event keys strictly after the edit point with the other keys and
/// markers, and refuses an event key in the span a negative ripple removes (before the fix event
/// keys stayed put and a later cue drifted against the picture).
#[test]
fn a_ripple_shifts_the_later_event_keys_and_refuses_one_in_the_removed_span() {
    let sequence = cue_sequence();
    let mut grown = sequence.clone();
    ripple_shift(&mut grown, 1.0, 1.0).expect("growth");
    assert_eq!(
        events_of(&grown, 1).iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        [0.5, 1.0, 3.0],
        "only the key strictly after 1 s moves"
    );
    assert_eq!(key_times(&grown, 0), Some(vec![0.0, 1.0]));

    let mut shrunk = sequence.clone();
    ripple_shift(&mut shrunk, 1.5, -0.25).expect("shrink");
    assert_eq!(
        events_of(&shrunk, 1).iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        [0.5, 1.0, 1.75]
    );

    let mut tight = sequence.clone();
    let err = ripple_shift(&mut tight, 2.5, -1.0).expect_err("`end` at 2 s is in (1.5, 2.5]");
    assert_eq!(err.code, "seq_ripple_keys_in_span");
    assert!(err.message.contains("cues"), "{}", err.message);
    assert_eq!(tight, sequence, "a refused ripple changes nothing");
}

/// ADR-8628: a clip of event keys (another sequence's clipboard, built here by hand) pastes its keys
/// into the event track of the same name. Before the fix the clip took the section path
/// (`Channel::is_keyed` is `false` for `Event`) and the paste returned no marks and changed nothing.
#[test]
fn an_event_clip_pastes_its_keys_into_the_track_of_its_name() {
    let mut other = Sequence {
        tracks: vec![
            event_track("cues", &[(0.0, "start")]),
            bool_track("c", &[(0.0, false)]),
        ],
        ..Sequence::default()
    };
    let hand = KeyClip {
        tracks: vec![ClipTrack {
            name: "cues".into(),
            target: String::new(),
            property: String::new(),
            keys: Channel::Event(vec![
                crate::sequencer::asset::EventKey::new(0.0, "boom"),
                crate::sequencer::asset::EventKey::new(0.25, "dust"),
            ]),
        }],
    };
    let marks = paste_marks(&mut other, &hand, 1.0).expect("hand-built clip pastes");
    assert_eq!(marks, [key(0, 1), key(0, 2)]);
    assert_eq!(
        events_of(&other, 0),
        [
            (0.0, "start".to_string()),
            (1.0, "boom".to_string()),
            (1.25, "dust".to_string())
        ]
    );
}
