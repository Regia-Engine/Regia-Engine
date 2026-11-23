// SPDX-License-Identifier: MPL-2.0
//! The motion-database editor, headless, on the procedural locomotion set: ops write the canonical
//! bytes, saved files load through the runtime loader, 50 random ops undo back to the original
//! bytes, the rebuild's warnings are the runtime builder's, the rebuild job cancels, and the agent
//! methods through the installed dispatcher table.

use std::path::{Path, PathBuf};

use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
use regia_animation::motion_matching::procedural::procedural_locomotion;
use regia_animation::motion_matching::{
    EntrySource, FeatureGroup, MAX_GROUP_WEIGHT, MotionDatabaseSource, SpanKindSource, SpanSource,
};
use regia_asset_pipeline::motiondb_cook::{MemoryInputs, cook_motiondb};
use regia_core::math::{Quat, Vec3};
use serde_json::{Value, json};

use super::model::{issues, lints};
use super::ops::{MmDbOp, OP_NAMES, apply_op, weight_mut};
use super::*;

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    world.insert_resource(MmDbMemoryInputs(MemoryInputs::procedural(
        &procedural_locomotion(),
    )));
    world
}

fn temp(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("mm-db-editor-{label}"))
}

/// Writes the procedural set's source as canonical text at `dir/loco.motiondb.ron`.
fn write_source(dir: &Path, source: &MotionDatabaseSource) -> PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join("loco.motiondb.ron");
    std::fs::write(&path, source.to_ron_string().expect("ron")).expect("write");
    path
}

fn doc(world: &World) -> MotionDatabaseSource {
    world
        .get_resource::<MmDbEditor>()
        .and_then(|e| e.doc.clone())
        .expect("open document")
}

fn undo_all(world: &mut World) {
    while crate::command::undo_world(world) {}
}

/// Acceptance: ops through the editor give exactly the bytes the runtime's canonical writer gives
/// for the expected asset, and the runtime loader and the cook accept the saved file.
#[test]
fn ops_write_the_canonical_bytes_of_the_expected_asset() {
    let set = procedural_locomotion();
    let dir = temp("canonical");
    let path = write_source(&dir, &set.source);
    let mut world = world();
    open(&mut world, &path, false).expect("open");
    let ops = vec![
        MmDbOp::SetWeight {
            group: "foot_velocity".into(),
            value: 2.5,
        },
        MmDbOp::SetWeight {
            group: "root_velocity".into(),
            value: 5000.0,
        },
        MmDbOp::SetHistoryTimes {
            times: vec![0.25, 0.5],
        },
        MmDbOp::SetSampleWeight {
            list: "history".into(),
            index: 1,
            value: 250.0,
        },
        MmDbOp::AddPoseJoint { joint: "root".into() },
        MmDbOp::RenameTag {
            name: "walk".into(),
            to: "stroll".into(),
        },
        MmDbOp::AddClipTag {
            index: 0,
            tag: "calm".into(),
        },
        MmDbOp::SetClip {
            index: 1,
            clip: None,
            looping: Some(false),
            mirrored: None,
            distance_matched: Some(true),
            trim: Some((-1.0, 0.1)),
        },
        MmDbOp::AddClip {
            clip: set.source.entries[0].clip.clone(),
            index: None,
        },
        MmDbOp::MoveClip { from: 0, to: 2 },
    ];
    for op in &ops {
        edit(&mut world, op.name(), None, std::slice::from_ref(op)).expect("op");
    }
    assert!(world.resource::<MmDbEditor>().is_dirty());
    save(&mut world, None).expect("save");
    assert!(!world.resource::<MmDbEditor>().is_dirty());

    let mut expected = set.source.clone();
    expected.schema.weights.foot_velocity = 2.5;
    expected.schema.weights.root_velocity = MAX_GROUP_WEIGHT;
    expected.schema.history_times = vec![0.25, 0.5];
    expected.schema.sample_weights.history = vec![1.0, 100.0];
    expected
        .schema
        .pose_joints
        .push(regia_animation::motion_matching::PoseJointSource {
            joint: "root".into(),
            position: true,
            velocity: true,
            weight: 1.0,
        });
    for tag in expected.tags.iter_mut().filter(|t| *t == "walk") {
        *tag = "stroll".into();
    }
    for entry in &mut expected.entries {
        for tag in entry.tags.iter_mut().filter(|t| *t == "walk") {
            *tag = "stroll".into();
        }
    }
    expected.tags.push("calm".into());
    expected.entries[0].tags.push("calm".into());
    expected.entries[1].looping = false;
    expected.entries[1].distance_matched = true;
    expected.entries[1].trim = (0.0, 0.1);
    expected.entries.push(EntrySource {
        clip: set.source.entries[0].clip.clone(),
        looping: false,
        tags: Vec::new(),
        mirrored: false,
        trim: (0.0, 0.0),
        spans: Vec::new(),
        distance_matched: false,
    });
    let moved = expected.entries.remove(0);
    expected.entries.insert(2, moved);

    let bytes = std::fs::read(&path).expect("saved");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        expected.to_ron_string().expect("expected ron"),
        "ops write the canonical bytes of the expected asset"
    );
    let text = String::from_utf8(bytes).expect("utf8");
    let loaded = MotionDatabaseSource::from_ron_str(&text).expect("runtime loader");
    assert_eq!(loaded, expected);
    let mut inputs = MemoryInputs::procedural(&set);
    cook_motiondb(&text, &mut inputs).expect("the cook builds the saved file");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tiny deterministic generator (no wall clock, no OS entropy).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn unit(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0
    }
}

fn random_op(rng: &mut Lcg, doc: &MotionDatabaseSource) -> MmDbOp {
    let entries = doc.entries.len().max(1);
    let tag = |rng: &mut Lcg| {
        if doc.tags.is_empty() || rng.below(3) == 0 {
            format!("t{}", rng.below(40))
        } else {
            doc.tags[rng.below(doc.tags.len())].clone()
        }
    };
    match rng.below(14) {
        0 => MmDbOp::SetWeight {
            group: FeatureGroup::ALL[rng.below(FeatureGroup::COUNT)].name().into(),
            value: rng.unit() * 2000.0 - 200.0,
        },
        1 => MmDbOp::SetSampleRate {
            rate: 10.0 + rng.unit() * 50.0,
        },
        2 => MmDbOp::SetTrajectoryTimes {
            times: (0..1 + rng.below(4)).map(|i| 0.2 * (i + 1) as f32).collect(),
        },
        3 => MmDbOp::SetHistoryTimes {
            times: (0..rng.below(3)).map(|i| 0.1 * (i + 1) as f32).collect(),
        },
        4 => MmDbOp::AddTag { name: tag(rng) },
        5 => MmDbOp::RemoveTag { name: tag(rng) },
        6 => MmDbOp::RenameTag {
            name: tag(rng),
            to: tag(rng),
        },
        7 => MmDbOp::AddClipTag {
            index: rng.below(entries),
            tag: tag(rng),
        },
        8 => MmDbOp::RemoveClipTag {
            index: rng.below(entries),
            tag: tag(rng),
        },
        9 => MmDbOp::SetClip {
            index: rng.below(entries),
            clip: None,
            looping: Some(rng.below(2) == 0),
            mirrored: Some(rng.below(2) == 0),
            distance_matched: Some(rng.below(2) == 0),
            trim: Some((rng.unit(), rng.unit())),
        },
        10 => MmDbOp::AddClip {
            clip: format!("anim/c{}.clip", rng.below(9)),
            index: Some(rng.below(entries + 1)),
        },
        11 => MmDbOp::RemoveClip {
            index: rng.below(entries),
        },
        12 => MmDbOp::MoveClip {
            from: rng.below(entries),
            to: rng.below(entries),
        },
        _ => MmDbOp::SetSampleWeight {
            list: "trajectory".into(),
            index: rng.below(doc.schema.trajectory_times.len().max(1)),
            value: rng.unit() * 150.0,
        },
    }
}

/// Acceptance: 50 random ops (each its own undo step), then undo to the bottom of the stack, and
/// the saved bytes are the original file's bytes. Every intermediate save loads through the
/// runtime loader.
#[test]
fn fifty_random_ops_then_full_undo_return_the_original_bytes() {
    let set = procedural_locomotion();
    let dir = temp("random-undo");
    let path = write_source(&dir, &set.source);
    let original = std::fs::read(&path).expect("original");
    let mut world = world();
    open(&mut world, &path, false).expect("open");
    let mut rng = Lcg(0x5eed_2026_1115);
    let mut applied = 0;
    let mut tries = 0;
    while applied < 50 {
        tries += 1;
        assert!(tries < 5000, "the generator keeps producing refused ops");
        let op = random_op(&mut rng, &doc(&world));
        if edit(&mut world, op.name(), None, &[op]).unwrap_or(false) {
            applied += 1;
            if applied % 10 == 0 {
                let probe = dir.join(format!("probe{applied}.motiondb.ron"));
                std::fs::write(&probe, canonical_text(&doc(&world)).expect("text")).expect("probe");
                let text = std::fs::read_to_string(&probe).expect("probe");
                MotionDatabaseSource::from_ron_str(&text).expect("every saved file loads");
            }
        }
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 50);
    undo_all(&mut world);
    assert!(
        !world.resource::<MmDbEditor>().is_dirty(),
        "full undo is the opened document"
    );
    save(&mut world, None).expect("save");
    assert_eq!(std::fs::read(&path).expect("saved"), original);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The skate clip of the runtime's own missing-root-motion test: the root moves 1 m/s while the
/// feet are keyed against it (procedural rig: root 0, feet 1 and 2).
fn skate() -> AnimationClip {
    let times: Vec<f32> = (0..=60).map(|k| k as f32 / 60.0).collect();
    let mut clip = AnimationClip::empty("skate", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(0.0, 0.0, t)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    for (joint, x) in [(1u32, -0.1f32), (2, 0.1)] {
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
    clip
}

/// Acceptance: a database with a clip that has no tags and a zero-weight feature group; the
/// rebuild job's warnings are exactly the runtime builder's (`MotionDatabaseSource::build`), and
/// the two authoring findings are the editor's checks, not build warnings. Nothing is written.
#[test]
fn rebuild_warnings_match_the_runtime_builder() {
    let set = procedural_locomotion();
    let mut source = set.source.clone();
    *weight_mut(&mut source.schema.weights, FeatureGroup::FootVelocity) = 0.0;
    source.entries.push(EntrySource {
        clip: "anim/skate.clip".into(),
        looping: false,
        tags: Vec::new(),
        mirrored: false,
        trim: (0.0, 0.0),
        spans: Vec::new(),
        distance_matched: false,
    });
    let dir = temp("rebuild");
    let path = write_source(&dir, &source);
    let mut world = world();
    world.insert_resource(MmDbMemoryInputs(
        MemoryInputs::procedural(&set).with_clip("anim/skate.clip", skate()),
    ));
    open(&mut world, &path, false).expect("open");
    let before: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    let id = start_rebuild(&mut world).expect("start");
    assert!(id.starts_with("mm_db_rebuild:"), "{id}");
    let view = finish_now(&mut world).expect("ran");
    let RebuildView::Done { report, .. } = view else {
        panic!("rebuild failed: {view:?}");
    };
    let mut clips = std::collections::BTreeMap::new();
    for (p, c) in &set.clips {
        clips.insert(p.clone(), c.clone());
    }
    clips.insert("anim/skate.clip".to_string(), skate());
    let runtime = source
        .build(&set.skeleton, |p| clips.get(p))
        .expect("runtime build");
    let runtime_warnings: Vec<(&str, usize)> = runtime
        .warnings()
        .iter()
        .map(|w| (w.kind.name(), w.clip))
        .collect();
    let editor_warnings: Vec<(&str, usize)> = report.warnings.iter().map(|w| (w.kind, w.clip)).collect();
    assert_eq!(editor_warnings, runtime_warnings);
    assert!(
        editor_warnings.contains(&("missing_root_motion", set.clips.len())),
        "the skate clip warns: {editor_warnings:?}"
    );
    assert_eq!(report.frames, runtime.len());
    assert_eq!(report.content_hash, runtime.content_hash());
    let codes: Vec<(&str, Option<usize>)> = lints(&source).iter().map(|i| (i.code, i.entry)).collect();
    assert_eq!(
        codes,
        [("untagged_clip", Some(set.clips.len())), ("zero_weight", None)],
        "the editor's checks"
    );
    assert!(issues(&source).is_empty(), "{:?}", issues(&source));
    let coverage: usize = report.coverage.iter().map(|c| c.frames).sum();
    assert_eq!(coverage, report.frames);
    assert_eq!(
        report.untagged_frames,
        report.coverage.last().map_or(0, |c| c.frames)
    );
    let after: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(before, after, "the rebuild writes nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The rebuild is an editor job: a cancel ends it `cancelled` (the agent job registry agrees), a
/// second start while one runs is refused, and a failing build names its code.
#[test]
fn rebuild_is_a_cancellable_job() {
    let set = procedural_locomotion();
    let dir = temp("cancel");
    let path = write_source(&dir, &set.source);
    let mut world = world();
    assert_eq!(
        start_rebuild(&mut world).expect_err("closed").code,
        "no_database_open"
    );
    open(&mut world, &path, false).expect("open");
    let id = start_rebuild(&mut world).expect("start");
    assert_eq!(
        start_rebuild(&mut world).expect_err("running").code,
        "rebuild_running"
    );
    assert!(cancel_rebuild(&mut world));
    assert!(!world.contains_resource::<MmDbRebuildJob>());
    assert_eq!(
        world.resource::<MmDbEditor>().rebuild,
        Some(RebuildView::Cancelled)
    );
    let jobs = world.resource::<crate::editor_jobs::EditorJobs>();
    assert_eq!(
        jobs.get(&id).map(regia_agent::jobs::JobHandle::state),
        Some(regia_agent::jobs::JobState::Cancelled)
    );
    edit(
        &mut world,
        "clip",
        None,
        &[MmDbOp::SetClip {
            index: 0,
            clip: Some("anim/missing.clip".into()),
            looping: None,
            mirrored: None,
            distance_matched: None,
            trim: None,
        }],
    )
    .expect("edit");
    start_rebuild(&mut world).expect("start");
    match finish_now(&mut world) {
        Some(RebuildView::Failed(e)) => assert_eq!(e.code, "unknown_clip", "{}", e.message),
        other => panic!("expected a failure, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review sprint-w11-rev-a-k #6: the editor's per-frame job poll (`editor_jobs::tick`, run in
/// `Last` with or without a shell window) ends a finished rebuild: the job row reports done, the
/// resource is gone and the next start is not refused `rebuild_running`.
#[test]
fn the_editor_job_poll_ends_a_finished_rebuild_without_the_window() {
    let set = procedural_locomotion();
    let dir = temp("tick");
    let path = write_source(&dir, &set.source);
    let mut world = world();
    open(&mut world, &path, false).expect("open");
    let id = start_rebuild(&mut world).expect("start");
    // Wait for the worker without taking its outcome (the poll applies it).
    world.resource_mut::<MmDbRebuildJob>().job.wait();
    crate::editor_jobs::tick(&mut world);
    assert!(
        !world.contains_resource::<MmDbRebuildJob>(),
        "the poll ended the rebuild"
    );
    let jobs = world.resource::<crate::editor_jobs::EditorJobs>();
    assert_eq!(
        jobs.get(&id).map(regia_agent::jobs::JobHandle::state),
        Some(regia_agent::jobs::JobState::Done)
    );
    start_rebuild(&mut world).expect("a new start is not refused");
    let _ = finish_now(&mut world);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The validator lists the runtime's own errors; ops clamp to its ranges and refuse what has no
/// range.
#[test]
fn the_validator_lists_runtime_errors_and_ops_clamp() {
    let set = procedural_locomotion();
    let mut doc = set.source.clone();
    assert!(issues(&doc).is_empty());
    doc.schema.weights.foot_position = -1.0;
    doc.entries[0].tags.push("nope".into());
    doc.entries[1].mirrored = true;
    doc.entries[2].spans.push(SpanSource {
        kind: SpanKindSource::LandingBias(500.0),
        range: (0.0, 0.1),
    });
    doc.schema.feet.push("foot_l".into());
    let codes: Vec<&str> = issues(&doc).iter().map(|i| i.code).collect();
    assert!(codes.contains(&"bad_weight"), "{codes:?}");
    assert!(codes.contains(&"unknown_tag"), "{codes:?}");
    assert!(codes.contains(&"no_mirror_table"), "{codes:?}");
    assert!(codes.contains(&"bad_span"), "{codes:?}");

    let mut doc = set.source.clone();
    apply_op(
        &mut doc,
        &MmDbOp::SetWeight {
            group: "foot_position".into(),
            value: -3.0,
        },
    )
    .expect("clamped");
    assert_eq!(doc.schema.weights.foot_position, 0.0);
    apply_op(
        &mut doc,
        &MmDbOp::SetPoseJoint {
            index: 0,
            position: None,
            velocity: None,
            weight: None,
        },
    )
    .expect_err("no pose joint");
    let refused = |op: MmDbOp| apply_op(&mut set.source.clone(), &op).expect_err("refused").code;
    assert_eq!(
        refused(MmDbOp::SetWeight {
            group: "elbow".into(),
            value: 1.0
        }),
        "unknown_group"
    );
    assert_eq!(
        refused(MmDbOp::SetWeight {
            group: "root_velocity".into(),
            value: f32::NAN
        }),
        "bad_value"
    );
    assert_eq!(refused(MmDbOp::SetSampleRate { rate: 0.0 }), "bad_value");
    assert_eq!(
        refused(MmDbOp::SetTrajectoryTimes { times: vec![0.5; 17] }),
        "too_many"
    );
    assert_eq!(
        refused(MmDbOp::SetHistoryTimes { times: vec![-0.1] }),
        "bad_value"
    );
    assert_eq!(refused(MmDbOp::AddTag { name: "walk".into() }), "duplicate");
    assert_eq!(refused(MmDbOp::RemoveTag { name: "nope".into() }), "unknown_tag");
    assert_eq!(refused(MmDbOp::RemoveClip { index: 99 }), "bad_index");
    assert_eq!(
        refused(MmDbOp::SetContacts {
            contacts: Some(regia_animation::motion_matching::ContactSettings {
                height_on: f32::INFINITY,
                ..Default::default()
            })
        }),
        "bad_value"
    );
    let mut doc = set.source.clone();
    apply_op(
        &mut doc,
        &MmDbOp::SetContacts {
            contacts: Some(regia_animation::motion_matching::ContactSettings {
                height_on: 0.1,
                height_off: 0.01,
                speed_on: -1.0,
                speed_off: 0.2,
                phase: false,
            }),
        },
    )
    .expect("clamped");
    let c = doc.schema.contacts.expect("contacts");
    assert_eq!((c.height_off, c.speed_on), (0.1, 0.0));
    assert!(issues(&doc).is_empty(), "{:?}", issues(&doc));
}

/// Tags: rename and remove reach every chip and every tag span; a chip of a new tag adds it.
#[test]
fn tag_ops_reach_every_entry_and_span() {
    let mut doc = procedural_locomotion().source;
    doc.entries[0].spans.push(SpanSource {
        kind: SpanKindSource::Tag(vec!["walk".into(), "stop".into()]),
        range: (0.0, 0.2),
    });
    apply_op(
        &mut doc,
        &MmDbOp::RenameTag {
            name: "walk".into(),
            to: "amble".into(),
        },
    )
    .expect("rename");
    assert!(!doc.tags.contains(&"walk".to_string()));
    assert!(doc.entries.iter().all(|e| !e.tags.contains(&"walk".to_string())));
    assert_eq!(
        doc.entries[0].spans[0].kind,
        SpanKindSource::Tag(vec!["amble".into(), "stop".into()])
    );
    apply_op(&mut doc, &MmDbOp::RemoveTag { name: "stop".into() }).expect("remove");
    assert_eq!(
        doc.entries[0].spans[0].kind,
        SpanKindSource::Tag(vec!["amble".into()])
    );
    assert!(issues(&doc).is_empty(), "{:?}", issues(&doc));
    apply_op(
        &mut doc,
        &MmDbOp::AddClipTag {
            index: 1,
            tag: "brisk".into(),
        },
    )
    .expect("chip");
    assert!(doc.tags.contains(&"brisk".to_string()));
    assert!(issues(&doc).is_empty());
}

/// Open, dirty, save, comments: a commented file warns; save drops the comments; undo past a save
/// makes the document dirty again; opening over unsaved work needs discard.
#[test]
fn open_save_dirty_and_comments() {
    let set = procedural_locomotion();
    let dir = temp("dirty");
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("c.motiondb.ron");
    let text = format!("// hero locomotion\n{}", set.source.to_ron_string().expect("ron"));
    std::fs::write(&path, text).expect("write");
    let mut world = world();
    open(&mut world, &path, false).expect("open");
    assert!(world.resource::<MmDbEditor>().comments_warning);
    assert!(!world.resource::<MmDbEditor>().is_dirty());
    edit(&mut world, "rate", None, &[MmDbOp::SetSampleRate { rate: 60.0 }]).expect("edit");
    assert!(world.resource::<MmDbEditor>().is_dirty());
    assert_eq!(
        open(&mut world, &path, false).expect_err("dirty").code,
        "unsaved_changes"
    );
    save(&mut world, None).expect("save");
    let saved = std::fs::read_to_string(&path).expect("saved");
    assert!(!saved.contains("//"), "the canonical text has no comments");
    assert!(!world.resource::<MmDbEditor>().comments_warning);
    assert!(crate::command::undo_world(&mut world));
    assert!(
        world.resource::<MmDbEditor>().is_dirty(),
        "undo past a save is dirty"
    );
    assert!(crate::command::redo_world(&mut world));
    assert!(!world.resource::<MmDbEditor>().is_dirty());
    assert_eq!(
        open(&mut world, &dir.join("x.bt.ron"), true)
            .expect_err("ext")
            .code,
        "bad_path"
    );
    assert_eq!(
        save(&mut World::new(), None).expect_err("none open").code,
        "no_database_open"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs one editor agent method through its installed handler (the dispatcher's table).
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

fn reason(e: &regia_agent::protocol::RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or("")
        .to_string()
}

/// The agent surface end to end through the installed method table: Content ▸ New's kind,
/// open, edit (one undo step), state, rebuild, save; every refusal has its code.
#[test]
fn agent_methods_through_the_dispatcher() {
    let root = temp("agent");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    let mut world = world();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let made = call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "motion_database", "path": "assets/anim/hero.motiondb.ron" }),
    )
    .expect("create");
    assert_eq!(made["path"], "assets/anim/hero.motiondb.ron", "{made}");
    let text = std::fs::read_to_string(root.join("assets/anim/hero.motiondb.ron")).expect("created");
    assert_eq!(
        MotionDatabaseSource::from_ron_str(&text).expect("loads"),
        new_document("")
    );

    for method in ["editor.mm_db_edit", "editor.mm_db_save"] {
        let err = call(
            &mut world,
            method,
            json!({ "ops": [{ "op": "add_tag", "name": "a" }] }),
        )
        .expect_err(method);
        assert_eq!(reason(&err), "no_database_open", "{method}");
    }
    let err = call(&mut world, "editor.mm_db_rebuild", json!({})).expect_err("closed");
    assert_eq!(reason(&err), "no_database_open");
    let err = call(
        &mut world,
        "editor.mm_db_open",
        json!({ "path": "assets/a.bt.ron" }),
    )
    .expect_err("ext");
    assert_eq!(reason(&err), "bad_path");
    let err = call(&mut world, "editor.mm_db_open", json!({})).expect_err("no path");
    assert_eq!(reason(&err), "missing_param");

    let opened = call(
        &mut world,
        "editor.mm_db_open",
        json!({ "path": "assets/anim/hero.motiondb.ron" }),
    )
    .expect("open");
    assert_eq!(opened["open"], true);
    assert_eq!(opened["dirty"], false);
    let codes: Vec<&str> = opened["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|i| i["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"no_skeleton") && codes.contains(&"no_clips"),
        "{codes:?}"
    );

    let set = procedural_locomotion();
    let mut ops = vec![
        json!({ "op": "set_skeleton", "path": set.source.skeleton }),
        json!({ "op": "set_feet", "feet": ["foot_l", "foot_r"] }),
        json!({ "op": "add_tag", "name": "walk" }),
    ];
    for entry in &set.source.entries {
        ops.push(json!({ "op": "add_clip", "clip": entry.clip }));
    }
    ops.push(json!({ "op": "add_clip_tag", "index": 1, "tag": "walk" }));
    ops.push(json!({ "op": "set_clip", "index": 1, "looping": true }));
    let edited = call(&mut world, "editor.mm_db_edit", json!({ "ops": ops })).expect("edit");
    assert_eq!(edited["changed"], true);
    assert_eq!(edited["dirty"], true);
    assert_eq!(edited["undo_depth"], 1, "one call is one undo step");
    assert_eq!(edited["valid"], true, "{}", edited["issues"]);
    assert_eq!(edited["document"]["entries"][1]["tags"], json!(["walk"]));

    let err = call(
        &mut world,
        "editor.mm_db_edit",
        json!({ "ops": [{ "op": "add_tag", "name": "x" }, { "op": "remove_clip", "index": 99 }] }),
    )
    .expect_err("all or nothing");
    assert_eq!(reason(&err), "bad_index");
    let err = call(
        &mut world,
        "editor.mm_db_edit",
        json!({ "ops": [{ "op": "fly" }] }),
    )
    .expect_err("bad op");
    assert_eq!(reason(&err), "bad_op");
    let state = call(&mut world, "editor.mm_db_state", json!({})).expect("state");
    assert_eq!(
        state["document"]["tags"],
        json!(["walk"]),
        "the refused batch changed nothing"
    );

    let rebuilt = call(&mut world, "editor.mm_db_rebuild", json!({})).expect("rebuild");
    assert_eq!(rebuilt["rebuild"]["state"], "done", "{}", rebuilt["rebuild"]);
    assert_eq!(rebuilt["rebuild"]["stale"], false);
    assert!(rebuilt["rebuild"]["report"]["frames"].as_u64().unwrap_or(0) > 0);
    assert!(
        rebuilt["job"]
            .as_str()
            .is_some_and(|j| j.starts_with("mm_db_rebuild:"))
    );
    let err = call(&mut world, "editor.mm_db_rebuild", json!({ "action": "cancel" })).expect_err("none");
    assert_eq!(reason(&err), "no_rebuild");
    let err = call(&mut world, "editor.mm_db_rebuild", json!({ "action": "explode" })).expect_err("bad");
    assert_eq!(reason(&err), "bad_action");
    let started = call(&mut world, "editor.mm_db_rebuild", json!({ "action": "start" })).expect("start");
    let job = started["job"].as_str().expect("job").to_string();
    call(&mut world, "editor.cancel_job", json!({ "id": job })).expect("editor.cancel_job reaches it");
    let state = call(&mut world, "editor.mm_db_state", json!({})).expect("state");
    assert_eq!(state["rebuild"]["state"], "cancelled", "{}", state["rebuild"]);

    let err = call(
        &mut world,
        "editor.mm_db_open",
        json!({ "path": "assets/anim/hero.motiondb.ron" }),
    )
    .expect_err("dirty");
    assert_eq!(reason(&err), "unsaved_changes");
    let saved = call(&mut world, "editor.mm_db_save", json!({})).expect("save");
    assert_eq!(saved["dirty"], false);
    let text = std::fs::read_to_string(root.join("assets/anim/hero.motiondb.ron")).expect("saved");
    let loaded = MotionDatabaseSource::from_ron_str(&text).expect("runtime loader");
    let mut inputs = MemoryInputs::procedural(&set);
    cook_motiondb(&text, &mut inputs).expect("the cook builds it");
    assert_eq!(loaded.entries.len(), set.source.entries.len());

    call(&mut world, "editor.undo", json!({})).expect("undo");
    let state = call(&mut world, "editor.mm_db_state", json!({})).expect("state");
    assert_eq!(state["dirty"], true);
    assert_eq!(state["document"]["entries"], json!([]));
    let _ = std::fs::remove_dir_all(&root);
}

/// The `ops` parameter's description names every op, and the method list is the five routes plus
/// part B's scrub.
#[test]
fn the_schema_names_every_op_and_route() {
    for name in OP_NAMES {
        assert!(agent::OPS_DOC.contains(name), "{name} is documented");
    }
    let names: Vec<&str> = agent::methods().iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        [
            "editor.mm_db_open",
            "editor.mm_db_state",
            "editor.mm_db_edit",
            "editor.mm_db_save",
            "editor.mm_db_rebuild",
            "editor.mm_db_scrub"
        ]
    );
    for op in OP_NAMES {
        let parsed = serde_json::from_value::<MmDbOp>(json!({ "op": op }));
        if let Ok(op_value) = parsed {
            assert_eq!(op_value.name(), op);
        }
    }
}

/// The palette command shows and hides the window (its agent route is `editor.command`).
#[test]
fn the_palette_toggles_the_window() {
    let mut world = world();
    assert_eq!(
        crate::command_palette::run(&mut world, "view.toggle_motion_database").as_deref(),
        Some("Motion database editor shown")
    );
    assert!(world.resource::<MmDbEditor>().ui.window_open);
    let out = call(
        &mut world,
        "editor.command",
        json!({ "run": "view.toggle_motion_database" }),
    )
    .expect("command");
    assert!(!world.resource::<MmDbEditor>().ui.window_open, "{out}");
}
