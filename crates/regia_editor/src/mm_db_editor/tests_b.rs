// SPDX-License-Identifier: MPL-2.0
//! Part B of the motion-database editor (w11-motiondb-editor-b), headless on the procedural
//! locomotion set: the span timeline driven by synthetic egui input (drag an edge by N frames and
//! the saved span moves by N; the keyboard does the same), same-kind overlap merging per spec 52
//! § 2.5 (proved against the runtime bake), the scrubbed frame's feature vector against the runtime
//! builder's row, debounced live rebuilds, warning rows that select their clip, autosave recovery
//! of an unsaved span edit, and the agent routes for span ops and scrubbing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
use regia_animation::motion_matching::procedural::procedural_locomotion;
use regia_animation::motion_matching::{
    ContactSettings, EntrySource, MotionDatabaseSource, SpanKindSource, SpanSource,
};
use regia_asset_pipeline::motiondb_cook::MemoryInputs;
use regia_core::math::{Quat, Vec3};
use serde_json::{Value, json};

use super::live::{DEBOUNCE, MmDbLive, live_tick};
use super::ops::{MmDbOp, apply_ops, frame_time};
use super::timeline::{Geometry, Part, TimelineState, timeline_ui};
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
    regia_test_utils::unique_temp_dir(&format!("mm-db-editor-b-{label}"))
}

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

fn opened(label: &str, source: &MotionDatabaseSource) -> (PathBuf, World) {
    let dir = temp(label);
    let path = write_source(&dir, source);
    let mut world = world();
    open(&mut world, &path, false).expect("open");
    (path, world)
}

fn saved(path: &Path) -> MotionDatabaseSource {
    MotionDatabaseSource::from_ron_str(&std::fs::read_to_string(path).expect("saved")).expect("loads")
}

fn undo_depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

fn cleanup(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic egui input on the timeline widget
// ---------------------------------------------------------------------------------------------

/// The timeline of one entry alone on a screen.
struct Strip {
    ctx: egui::Context,
    entry: usize,
}

impl Strip {
    fn new(world: &mut World, entry: usize) -> Self {
        let mut strip = Self {
            ctx: egui::Context::default(),
            entry,
        };
        strip.frame(world, Vec::new());
        strip
    }

    fn frame(&mut self, world: &mut World, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 600.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let entry = self.entry;
        let mut output = self.ctx.run_ui(raw, |ui| {
            let mut state = std::mem::take(&mut world.resource_mut::<MmDbEditor>().ui.timeline);
            timeline_ui(ui, world, &mut state, entry);
            world.resource_mut::<MmDbEditor>().ui.timeline = state;
        });
        output.textures_delta.clear();
    }

    fn geometry(&self, world: &World) -> Geometry {
        *world
            .resource::<MmDbEditor>()
            .ui
            .timeline
            .geometry
            .get(&self.entry)
            .expect("the strip was drawn")
    }

    fn state<'w>(&self, world: &'w World) -> &'w TimelineState {
        &world.resource::<MmDbEditor>().ui.timeline
    }

    /// Presses at `from`, moves to `to` in four steps, releases.
    fn drag(&mut self, world: &mut World, from: egui::Pos2, to: egui::Pos2) {
        self.frame(world, vec![egui::Event::PointerMoved(from)]);
        self.frame(world, vec![button(from, true)]);
        for k in 1..=4 {
            let p = from + (to - from) * (k as f32 / 4.0);
            self.frame(world, vec![egui::Event::PointerMoved(p)]);
        }
        self.frame(world, vec![button(to, false)]);
        self.frame(world, Vec::new());
    }

    fn click(&mut self, world: &mut World, at: egui::Pos2) {
        self.frame(world, vec![egui::Event::PointerMoved(at)]);
        self.frame(world, vec![button(at, true)]);
        self.frame(world, vec![button(at, false)]);
        self.frame(world, Vec::new());
    }

    fn key(&mut self, world: &mut World, key: egui::Key, shift: bool) {
        let modifiers = if shift {
            egui::Modifiers::SHIFT
        } else {
            egui::Modifiers::NONE
        };
        self.frame(
            world,
            vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ],
        );
        self.frame(
            world,
            vec![
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers,
                },
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
    }
}

fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

fn edge(g: &Geometry, row: usize, boundary: u32) -> egui::Pos2 {
    egui::pos2(g.x_of(boundary), g.row_y(row))
}

/// Acceptance (1): synthetic input drags the end edge of a span by N frames, and the saved span's
/// end moves by exactly N frames (snapped to the runtime's frame times, the start kept), as one
/// undo step. A body drag moves both edges; a drag on the bottom row adds a span.
#[test]
fn dragging_a_span_edge_by_n_frames_moves_the_saved_span_by_n() {
    let set = procedural_locomotion();
    let mut source = set.source.clone();
    source.entries[0].spans.push(SpanSource {
        kind: SpanKindSource::Unindexed,
        range: (0.1, 0.3),
    });
    let (path, mut world) = opened("drag", &source);
    let rate = source.schema.sample_rate;
    let mut strip = Strip::new(&mut world, 0);
    let g = strip.geometry(&world);
    assert!(g.frames >= 20, "the strip shows the clip's frames: {g:?}");
    assert!(g.step() > 8.0, "frames are wide enough to aim at: {g:?}");
    let (a, b) = super::timeline::span_frames(rate, &source.entries[0].spans[0]);

    // End edge +4 frames.
    let n = 4;
    let depth = undo_depth(&world);
    strip.drag(&mut world, edge(&g, 1, b), edge(&g, 1, b + n));
    assert_eq!(undo_depth(&world), depth + 1, "one drag is one undo step");
    save(&mut world, None).expect("save");
    let span = &saved(&path).entries[0].spans[0];
    assert_eq!(span.range.0, 0.1, "the start did not move");
    assert_eq!(
        span.range.1,
        frame_time(rate, b + n),
        "the end moved by {n} frames"
    );
    assert_eq!(
        super::timeline::span_frames(rate, span),
        (a, b + n),
        "the saved span is {n} frames longer"
    );
    let selected = strip
        .state(&world)
        .selected
        .expect("the dragged span is selected");
    assert_eq!((selected.span, selected.part), (0, Part::End));

    // Body -2 frames: both edges move, the length stays.
    let g = strip.geometry(&world);
    let mid = egui::pos2((g.x_of(a) + g.x_of(b + n)) * 0.5, g.row_y(1));
    strip.drag(&mut world, mid, mid - egui::vec2(2.0 * g.step(), 0.0));
    save(&mut world, None).expect("save");
    let span = &saved(&path).entries[0].spans[0];
    assert_eq!(
        super::timeline::span_frames(rate, span),
        (a - 2, b + n - 2),
        "the body moved two frames"
    );
    assert_eq!(span.range, (frame_time(rate, a - 2), frame_time(rate, b + n - 2)));

    // A drag on the bottom row adds a span of the chosen kind.
    strip.frame(&mut world, Vec::new());
    world.resource_mut::<MmDbEditor>().ui.timeline.new_kind = super::timeline::NewKind::PassThrough;
    let g = strip.geometry(&world);
    let row = g.spans + 1;
    strip.drag(&mut world, edge(&g, row, 15), edge(&g, row, 19));
    let spans = doc(&world).entries[0].spans.clone();
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert_eq!(spans[1].kind, SpanKindSource::PassThrough);
    assert_eq!(spans[1].range, (frame_time(rate, 15), frame_time(rate, 19)));

    // Each of the three is one undo step back to the opened bytes.
    for _ in 0..3 {
        assert!(crate::command::undo_world(&mut world));
    }
    assert_eq!(doc(&world), source);
    cleanup(&path);
}

/// Acceptance (1), keyboard: with the span's end edge selected, Right moves it one frame and
/// Shift+Left ten (one undo step each); E switches to the body, Up/Down pick spans, N adds a span
/// at the cursor and Delete removes the selected one.
#[test]
fn arrow_keys_move_the_selected_edge_one_frame_shift_ten() {
    let set = procedural_locomotion();
    let mut source = set.source.clone();
    let rate = source.schema.sample_rate;
    source.entries[0].spans.push(SpanSource {
        kind: SpanKindSource::LandingBias(-0.5),
        range: (frame_time(rate, 4), frame_time(rate, 18)),
    });
    let (path, mut world) = opened("keys", &source);
    let mut strip = Strip::new(&mut world, 0);
    let g = strip.geometry(&world);
    // A click on the end edge selects it and focuses the strip.
    strip.click(&mut world, edge(&g, 1, 18));
    let selected = strip.state(&world).selected.expect("selected");
    assert_eq!((selected.span, selected.part), (0, Part::End));

    let depth = undo_depth(&world);
    strip.key(&mut world, egui::Key::ArrowRight, false);
    assert_eq!(doc(&world).entries[0].spans[0].range.1, frame_time(rate, 19));
    assert_eq!(undo_depth(&world), depth + 1, "one key press is one undo step");
    strip.key(&mut world, egui::Key::ArrowLeft, true);
    assert_eq!(
        doc(&world).entries[0].spans[0].range,
        (frame_time(rate, 4), frame_time(rate, 9)),
        "Shift+Left moves the end ten frames"
    );
    assert_eq!(undo_depth(&world), depth + 2);
    // The end never crosses the start.
    strip.key(&mut world, egui::Key::ArrowLeft, true);
    assert_eq!(doc(&world).entries[0].spans[0].range.1, frame_time(rate, 5));

    // E: from the end edge to the body.
    strip.key(&mut world, egui::Key::E, false);
    assert_eq!(strip.state(&world).selected.map(|s| s.part), Some(Part::Body));
    strip.key(&mut world, egui::Key::ArrowRight, false);
    assert_eq!(
        doc(&world).entries[0].spans[0].range,
        (frame_time(rate, 5), frame_time(rate, 6))
    );

    // N adds a span of the chosen kind at the cursor; Down selects it; Delete removes it.
    world.resource_mut::<MmDbEditor>().ui.timeline.cursor = Some(super::timeline::Cursor {
        entry: 0,
        frame: 12,
        mirrored: false,
    });
    strip.key(&mut world, egui::Key::N, false);
    let spans = doc(&world).entries[0].spans.clone();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[1].kind, SpanKindSource::Unindexed);
    assert_eq!(spans[1].range, (frame_time(rate, 12), frame_time(rate, 13)));
    strip.key(&mut world, egui::Key::ArrowUp, false);
    strip.key(&mut world, egui::Key::ArrowDown, false);
    assert_eq!(strip.state(&world).selected.map(|s| s.span), Some(1));
    strip.key(&mut world, egui::Key::Delete, false);
    assert_eq!(doc(&world).entries[0].spans.len(), 1);
    assert_eq!(strip.state(&world).selected, None);
    cleanup(&path);
}

/// Spec 52 § 2.5: "overlapping spans combine: biases add, masks OR, `unindexed` and
/// `pass_through` are sticky". So an `unindexed`, `pass_through` or same-tags `tag` span that
/// overlaps or touches another of its kind merges into their union (the baked frames are the same
/// either way, checked against the runtime build), while `landing_bias` spans stack.
#[test]
fn overlapping_spans_of_the_same_kind_merge_per_the_spec() {
    let set = procedural_locomotion();
    let rate = set.source.schema.sample_rate;
    let add = |kind: SpanKindSource, a: u32, b: u32| MmDbOp::AddSpan {
        index: 1,
        kind,
        range: None,
        frames: Some((a, b)),
    };
    let walk = || SpanKindSource::Tag(vec!["walk".into()]);
    let ops = [
        add(SpanKindSource::Unindexed, 0, 10),
        add(SpanKindSource::Unindexed, 8, 20),
        add(SpanKindSource::Unindexed, 20, 25),
        add(SpanKindSource::PassThrough, 3, 6),
        add(SpanKindSource::LandingBias(-1.0), 0, 10),
        add(SpanKindSource::LandingBias(-2.0), 5, 15),
        add(walk(), 0, 5),
        add(walk(), 3, 8),
        add(SpanKindSource::Tag(vec!["run".into()]), 4, 9),
    ];
    let merged = apply_ops(&set.source, &ops).expect("ops");
    let spans = &merged.entries[1].spans;
    let shape: Vec<(SpanKindSource, (u32, u32))> = spans
        .iter()
        .map(|s| (s.kind.clone(), super::timeline::span_frames(rate, s)))
        .collect();
    assert_eq!(
        shape,
        [
            (SpanKindSource::Unindexed, (0, 25)),
            (SpanKindSource::PassThrough, (3, 6)),
            (SpanKindSource::LandingBias(-1.0), (0, 10)),
            (SpanKindSource::LandingBias(-2.0), (5, 15)),
            (walk(), (0, 8)),
            (SpanKindSource::Tag(vec!["run".into()]), (4, 9)),
        ]
    );
    // The same spans unmerged bake the same frames in the runtime.
    let mut unmerged = set.source.clone();
    unmerged.entries[1].spans = ops
        .iter()
        .map(|op| match op {
            MmDbOp::AddSpan {
                kind,
                frames: Some((a, b)),
                ..
            } => SpanSource {
                kind: kind.clone(),
                range: (frame_time(rate, *a), frame_time(rate, *b)),
            },
            _ => unreachable!(),
        })
        .collect();
    let a = merged
        .build(&set.skeleton, |p| set.clip(p))
        .expect("merged builds");
    let b = unmerged
        .build(&set.skeleton, |p| set.clip(p))
        .expect("unmerged builds");
    assert_eq!(a.len(), b.len());
    for frame in 0..a.len() {
        assert_eq!(a.frame_meta(frame), b.frame_meta(frame), "frame {frame}");
    }
    // Moving a span onto another of its kind merges too; a bad range is refused.
    let moved = apply_ops(
        &merged,
        &[MmDbOp::SetSpan {
            index: 1,
            span: 1,
            kind: Some(SpanKindSource::Unindexed),
            range: None,
            frames: Some((25, 30)),
        }],
    )
    .expect("move");
    assert_eq!(
        super::timeline::span_frames(rate, &moved.entries[1].spans[0]),
        (0, 30)
    );
    assert_eq!(moved.entries[1].spans.len(), spans.len() - 1);
    for bad in [
        add(SpanKindSource::Unindexed, 5, 5),
        MmDbOp::AddSpan {
            index: 1,
            kind: SpanKindSource::Unindexed,
            range: Some((0.5, 0.2)),
            frames: None,
        },
        MmDbOp::AddSpan {
            index: 1,
            kind: SpanKindSource::Unindexed,
            range: Some((0.0, 0.2)),
            frames: Some((0, 1)),
        },
        add(SpanKindSource::LandingBias(f32::NAN), 0, 1),
    ] {
        let err = apply_ops(&set.source, std::slice::from_ref(&bad)).expect_err("refused");
        assert_eq!(err.error.code, "bad_value", "{bad:?}");
    }
    let clamped = apply_ops(&set.source, &[add(SpanKindSource::LandingBias(500.0), 0, 1)]).expect("clamped");
    assert_eq!(
        clamped.entries[1].spans[0].kind,
        SpanKindSource::LandingBias(regia_animation::motion_matching::MAX_BIAS)
    );
    let err = apply_ops(&set.source, &[MmDbOp::RemoveSpan { index: 1, span: 0 }]).expect_err("no span");
    assert_eq!(err.error.code, "bad_index");
}

// ---------------------------------------------------------------------------------------------
// Pose preview
// ---------------------------------------------------------------------------------------------

/// Acceptance (2): the scrubbed frame's feature vector is the runtime builder's row for that frame
/// (every entry, first/middle/last frames, the mirrored copy, with history samples and contact
/// labels in the layout), bit for bit; the frame's time and baked meta match too.
#[test]
fn the_scrubbed_frame_vector_equals_the_runtime_builders_row() {
    let set = procedural_locomotion();
    let mut source = set.source.clone();
    source.schema.history_times = vec![0.2, 0.4];
    source.schema.contacts = Some(ContactSettings::default());
    source.mirror_pairs = vec![("foot_l".into(), "foot_r".into())];
    source.entries[1].mirrored = true;
    source.entries[1].spans.push(SpanSource {
        kind: SpanKindSource::LandingBias(-3.0),
        range: (0.1, 0.5),
    });
    let (path, mut world) = opened("scrub", &source);
    let runtime = source
        .build(&set.skeleton, |p| set.clip(p))
        .expect("runtime build");
    let mut checked = 0;
    for range in runtime.clips() {
        for frame in [0, range.frames / 2, range.frames - 1] {
            let view = super::preview::frame_view(&mut world, range.source, frame, range.mirrored)
                .unwrap_or_else(|e| panic!("entry {} frame {frame}: {e:?}", range.source));
            let row = range.start + frame;
            assert_eq!(
                view.features.as_slice(),
                runtime.raw_features(row).expect("row"),
                "entry {} frame {frame} mirrored {}",
                range.source,
                range.mirrored
            );
            assert_eq!(view.time, runtime.frame(row).expect("frame").time);
            assert_eq!(view.frames, range.frames);
            assert_eq!(view.meta, runtime.frame_meta(row), "baked meta");
            assert_eq!(view.pose.len(), set.skeleton.len());
            checked += 1;
        }
    }
    assert_eq!(checked, runtime.clips().len() * 3);
    assert!(
        runtime.clips().iter().any(|r| r.mirrored),
        "a mirrored copy was checked"
    );
    // Past the end, and a mirrored frame of an entry that is not mirrored, are refused.
    let frames = runtime.clips()[0].frames;
    assert_eq!(
        super::preview::frame_view(&mut world, 0, frames, false)
            .expect_err("past the end")
            .code,
        "bad_frame"
    );
    assert_eq!(
        super::preview::frame_view(&mut world, 0, 0, true)
            .expect_err("not mirrored")
            .code,
        "bad_frame"
    );
    // A span edit rebuilds that entry only.
    let built = world.resource::<super::preview::MmDbPreview>().builds();
    edit(
        &mut world,
        "span",
        None,
        &[MmDbOp::AddSpan {
            index: 2,
            kind: SpanKindSource::Unindexed,
            range: None,
            frames: Some((0, 3)),
        }],
    )
    .expect("edit");
    let view = super::preview::frame_view(&mut world, 2, 1, false).expect("entry 2");
    assert!(view.meta.is_some_and(|m| !m.is_target()), "the new span bakes");
    assert_eq!(
        world.resource::<super::preview::MmDbPreview>().builds(),
        built + 1
    );
    super::preview::frame_view(&mut world, 0, 1, false).expect("entry 0");
    assert_eq!(
        world.resource::<super::preview::MmDbPreview>().builds(),
        built + 1,
        "the other entries come from the cache"
    );
    cleanup(&path);
}

// ---------------------------------------------------------------------------------------------
// Live stats and warnings
// ---------------------------------------------------------------------------------------------

/// The skate clip of part A's warning test: the root moves while the feet are keyed against it.
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

/// Acceptance (3): edits start no rebuild until the document has been unchanged for the debounce
/// time; then one rebuild job runs off the UI thread and the warnings list and stats follow the
/// edit (adding the skate clip adds its `missing_root_motion` warning); a warning row selects the
/// clip and frame it names. Instants are passed in: no wall clock.
#[test]
fn live_rebuild_is_debounced_and_refreshes_the_warnings() {
    let set = procedural_locomotion();
    let (path, mut world) = opened("live", &set.source);
    world.insert_resource(MmDbMemoryInputs(
        MemoryInputs::procedural(&set).with_clip("anim/skate.clip", skate()),
    ));
    let t0 = Instant::now();
    assert_eq!(
        live_tick(&mut world, t0),
        None,
        "the open revision starts the clock"
    );
    let id = live_tick(&mut world, t0 + DEBOUNCE).expect("the opened document rebuilds");
    assert!(id.starts_with("mm_db_rebuild:"), "{id}");
    finish_now(&mut world).expect("ran");
    let frames_before = match &world.resource::<MmDbEditor>().rebuild {
        Some(RebuildView::Done { report, .. }) => {
            assert!(report.warnings.is_empty(), "{:?}", report.warnings);
            report.frames
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(live_tick(&mut world, t0 + DEBOUNCE * 3), None, "nothing changed");

    // Three quick edits: no rebuild until the last has settled.
    let t1 = t0 + DEBOUNCE * 4;
    let step = DEBOUNCE / 2;
    let edits = [
        MmDbOp::AddClip {
            clip: "anim/skate.clip".into(),
            index: None,
        },
        MmDbOp::AddClipTag {
            index: set.source.entries.len(),
            tag: "walk".into(),
        },
        MmDbOp::SetClip {
            index: set.source.entries.len(),
            clip: None,
            looping: None,
            mirrored: None,
            distance_matched: Some(true),
            trim: None,
        },
    ];
    for (k, op) in edits.iter().enumerate() {
        edit(&mut world, op.name(), None, std::slice::from_ref(op)).expect("edit");
        let now = t1 + step * k as u32;
        assert_eq!(live_tick(&mut world, now), None, "edit {k} restarts the wait");
        assert!(!world.contains_resource::<MmDbRebuildJob>());
    }
    let last = t1 + step * 2;
    assert_eq!(
        live_tick(&mut world, last + DEBOUNCE - Duration::from_millis(1)),
        None
    );
    let id = live_tick(&mut world, last + DEBOUNCE).expect("settled: one rebuild");
    assert!(world.contains_resource::<MmDbRebuildJob>(), "{id} runs as a job");
    finish_now(&mut world).expect("ran");
    let report = match &world.resource::<MmDbEditor>().rebuild {
        Some(RebuildView::Done { report, revision }) => {
            assert_eq!(
                *revision,
                world.resource::<MmDbEditor>().revision,
                "built from the last edit"
            );
            report.clone()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(world.resource::<MmDbLive>().runs, 2);
    assert!(report.frames > frames_before, "the stats follow the edit");
    let warning = report
        .warnings
        .iter()
        .find(|w| w.kind == "missing_root_motion")
        .expect("the skate clip warns");
    assert_eq!(warning.entry, set.source.entries.len());

    // The warning row selects its clip (frame 0) and scrolls the list there.
    super::timeline::select_frame(&mut world, warning.entry, 0, false);
    let t = &world.resource::<MmDbEditor>().ui.timeline;
    assert_eq!(
        t.cursor,
        Some(super::timeline::Cursor {
            entry: warning.entry,
            frame: 0,
            mirrored: false
        })
    );
    assert_eq!(t.scroll_to, Some(warning.entry));

    // Live off: edits rebuild nothing.
    world.resource_mut::<MmDbLive>().enabled = false;
    edit(&mut world, "tag", None, &[MmDbOp::AddTag { name: "x".into() }]).expect("edit");
    assert_eq!(live_tick(&mut world, last + DEBOUNCE * 10), None);
    assert_eq!(live_tick(&mut world, last + DEBOUNCE * 20), None);
    cleanup(&path);
}

/// Review g16 row 11: a live rebuild that cannot start (no project and no inputs) is not latched;
/// once inputs exist, the next tick after the debounce starts it for the same revision.
#[test]
fn a_failed_live_start_is_retried_after_the_debounce() {
    let set = procedural_locomotion();
    let (path, mut world) = opened("live-retry", &set.source);
    let inputs = world
        .remove_resource::<MmDbMemoryInputs>()
        .expect("the test world has inputs");
    let t0 = Instant::now();
    assert_eq!(live_tick(&mut world, t0), None, "the clock starts");
    assert_eq!(
        live_tick(&mut world, t0 + DEBOUNCE),
        None,
        "no project and no inputs: the start fails"
    );
    assert!(
        world
            .resource::<MmDbLive>()
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("no_project")),
        "{:?}",
        world.resource::<MmDbLive>().last_error
    );
    assert!(!world.contains_resource::<MmDbRebuildJob>());
    world.insert_resource(inputs);
    assert_eq!(
        live_tick(&mut world, t0 + DEBOUNCE * 2),
        None,
        "the retry waits the debounce again"
    );
    let id = live_tick(&mut world, t0 + DEBOUNCE * 3).expect("the same revision is retried");
    assert!(id.starts_with("mm_db_rebuild:"), "{id}");
    assert_eq!(world.resource::<MmDbLive>().runs, 1);
    assert_eq!(world.resource::<MmDbLive>().last_error, None);
    finish_now(&mut world).expect("ran");
    assert_eq!(
        live_tick(&mut world, t0 + DEBOUNCE * 5),
        None,
        "a started revision is not rebuilt again"
    );
    cleanup(&path);
}

/// A `bad_span` problem names its span, so its row can select the span's start frame.
#[test]
fn a_bad_span_problem_names_its_span() {
    let set = procedural_locomotion();
    let mut source = set.source.clone();
    source.entries[2].spans = vec![
        SpanSource {
            kind: SpanKindSource::Unindexed,
            range: (0.0, 0.1),
        },
        SpanSource {
            kind: SpanKindSource::LandingBias(400.0),
            range: (0.2, 0.3),
        },
    ];
    let found = super::model::issues(&source);
    let issue = found.iter().find(|i| i.code == "bad_span").expect("bad bias");
    assert_eq!((issue.entry, issue.span), (Some(2), Some(1)));
    assert_eq!(issue.json()["span"], 1);
}

// ---------------------------------------------------------------------------------------------
// Autosave and recovery
// ---------------------------------------------------------------------------------------------

/// Acceptance (4): an unsaved span edit is in the `motion_db` autosave snapshot, and restoring it
/// in a fresh editor gives the edited document at its original path, dirty, with the window open.
#[test]
fn recovery_restores_an_unsaved_span_edit() {
    let set = procedural_locomotion();
    let (path, mut world) = opened("autosave", &set.source);
    crate::doc_autosave::install(&mut world);
    assert!(
        crate::doc_autosave::dirty_documents(&world)
            .iter()
            .all(|k| k.kind != "motion_db"),
        "a clean document has nothing to save"
    );
    edit(
        &mut world,
        "span",
        None,
        &[MmDbOp::AddSpan {
            index: 0,
            kind: SpanKindSource::PassThrough,
            range: None,
            frames: Some((2, 9)),
        }],
    )
    .expect("edit");
    let edited = doc(&world);
    let snapshots = crate::doc_autosave::take_snapshots(&world);
    let snap = snapshots
        .iter()
        .find(|s| s.key.kind == "motion_db")
        .expect("a motion_db snapshot");
    assert_eq!(snap.key.original.as_deref(), Some(path.as_path()));

    let mut fresh = World::new();
    crate::doc_autosave::install(&mut fresh);
    let provider = fresh
        .resource::<crate::doc_autosave::DocAutosaveRegistry>()
        .provider("motion_db")
        .expect("the motion_db provider is registered");
    (provider.restore)(&mut fresh, &snap.key, &snap.bytes).expect("restore");
    let editor = fresh.resource::<MmDbEditor>();
    assert_eq!(editor.doc.as_ref(), Some(&edited), "the span edit is back");
    assert!(editor.is_dirty(), "dirty until saved");
    assert_eq!(editor.path.as_deref(), Some(path.as_path()));
    assert!(editor.ui.window_open);
    // Saving the recovered document writes the span.
    save(&mut fresh, None).expect("save");
    assert_eq!(saved(&path).entries[0].spans, edited.entries[0].spans);
    // Garbage is refused, not half-opened.
    assert!((provider.restore)(&mut fresh, &snap.key, b"(nope").is_err());
    cleanup(&path);
}

// ---------------------------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------------------------

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

/// Acceptance (5): span ops and scrubbing through the installed method table: `add_span`,
/// `set_span` and `remove_span` in `editor.mm_db_edit` (one undo step), `editor.mm_db_scrub` by
/// frame or time (the vector is the runtime's row; the cursor moves), live on/off, and the
/// refusal codes.
#[test]
fn span_ops_and_scrub_through_the_dispatcher() {
    let set = procedural_locomotion();
    let (path, mut world) = opened("agent", &set.source);
    let rate = set.source.schema.sample_rate;
    let out = call(
        &mut world,
        "editor.mm_db_edit",
        json!({ "ops": [
            { "op": "add_span", "index": 0, "kind": "unindexed", "frames": [2, 6] },
            { "op": "add_span", "index": 0, "kind": { "landing_bias": -1.5 }, "range": [0.0, 0.2] },
            { "op": "add_span", "index": 0, "kind": { "tag": ["stop"] }, "frames": [10, 12] },
            { "op": "set_span", "index": 0, "span": 0, "frames": [2, 8] },
        ] }),
    )
    .expect("edit");
    assert_eq!(out["changed"], true);
    assert_eq!(undo_depth(&world), 1, "one call, one undo step");
    let spans = doc(&world).entries[0].spans.clone();
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[0].range, (frame_time(rate, 2), frame_time(rate, 8)));
    assert_eq!(spans[1].kind, SpanKindSource::LandingBias(-1.5));
    assert_eq!(spans[2].kind, SpanKindSource::Tag(vec!["stop".into()]));

    let scrub = call(
        &mut world,
        "editor.mm_db_scrub",
        json!({ "entry": 0, "frame": 3 }),
    )
    .expect("scrub");
    let runtime = doc(&world).build(&set.skeleton, |p| set.clip(p)).expect("build");
    let row = runtime.clips()[0].start + 3;
    let expected: Vec<f64> = runtime
        .raw_features(row)
        .expect("row")
        .iter()
        .map(|v| f64::from(*v))
        .collect();
    let got: Vec<f64> = scrub["features"]
        .as_array()
        .expect("features")
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    assert_eq!(got, expected);
    assert_eq!(scrub["meta"]["unindexed"], true, "{scrub}");
    assert_eq!(scrub["meta"]["target"], false);
    assert_eq!(scrub["meta"]["bias"], -1.5);
    assert_eq!(scrub["spans"][0]["frames"], json!([2, 8]));
    assert_eq!(
        world
            .resource::<MmDbEditor>()
            .ui
            .timeline
            .cursor
            .map(|c| (c.entry, c.frame)),
        Some((0, 3)),
        "the window's cursor follows"
    );
    let by_time = call(
        &mut world,
        "editor.mm_db_scrub",
        json!({ "entry": 0, "time": frame_time(rate, 11) }),
    )
    .expect("by time");
    assert_eq!(by_time["frame"], 11);
    let meta = runtime.frame_meta(runtime.clips()[0].start + 11).expect("meta");
    let names: Vec<&str> = runtime.tag_table().names_of(meta.tags);
    assert!(names.contains(&"stop"), "the tag span bakes: {names:?}");
    assert_eq!(by_time["meta"]["tags"], json!(names), "{by_time}");
    let state = call(&mut world, "editor.mm_db_state", json!({})).expect("state");
    assert_eq!(state["cursor"]["frame"], 11);

    let out = call(
        &mut world,
        "editor.mm_db_edit",
        json!({ "ops": [{ "op": "remove_span", "index": 0, "span": 1 }] }),
    )
    .expect("remove");
    assert_eq!(
        out["document"]["entries"][0]["spans"].as_array().map(Vec::len),
        Some(2)
    );
    call(&mut world, "editor.undo", json!({})).expect("undo");
    assert_eq!(doc(&world).entries[0].spans.len(), 3);

    let live = call(
        &mut world,
        "editor.mm_db_rebuild",
        json!({ "action": "live", "enabled": false }),
    )
    .expect("live off");
    assert_eq!(live["live"]["enabled"], false);
    assert!(!world.resource::<MmDbLive>().enabled);

    for (method, params, code) in [
        ("editor.mm_db_scrub", json!({}), "missing_param"),
        ("editor.mm_db_scrub", json!({ "entry": 99 }), "bad_index"),
        (
            "editor.mm_db_scrub",
            json!({ "entry": 0, "frame": 100000 }),
            "bad_frame",
        ),
        (
            "editor.mm_db_scrub",
            json!({ "entry": 0, "mirrored": true }),
            "bad_frame",
        ),
        (
            "editor.mm_db_scrub",
            json!({ "entry": 0, "frame": 1, "time": 0.1 }),
            "bad_value",
        ),
        (
            "editor.mm_db_scrub",
            json!({ "entry": 0, "time": -1.0 }),
            "bad_value",
        ),
        (
            "editor.mm_db_rebuild",
            json!({ "action": "live" }),
            "missing_param",
        ),
        (
            "editor.mm_db_edit",
            json!({ "ops": [{ "op": "add_span", "index": 0, "kind": "unindexed" }] }),
            "bad_value",
        ),
        (
            "editor.mm_db_edit",
            json!({ "ops": [{ "op": "set_span", "index": 0, "span": 9, "frames": [0, 1] }] }),
            "bad_index",
        ),
        (
            "editor.mm_db_edit",
            json!({ "ops": [{ "op": "add_span", "index": 0, "kind": "sideways", "frames": [0, 1] }] }),
            "bad_op",
        ),
    ] {
        let err = call(&mut world, method, params.clone()).expect_err("refused");
        assert_eq!(reason(&err), code, "{method} {params}");
    }
    let mut closed = self::world();
    let err = call(&mut closed, "editor.mm_db_scrub", json!({ "entry": 0 })).expect_err("closed");
    assert_eq!(reason(&err), "no_database_open");
    cleanup(&path);
}

/// The autosave registry lists the motion database editor.
#[test]
fn the_motion_db_autosave_provider_is_registered() {
    let mut registry = crate::doc_autosave::DocAutosaveRegistry::default();
    crate::doc_autosave_providers::register_builtin(&mut registry);
    assert!(registry.kinds().contains(&"motion_db"), "{:?}", registry.kinds());
}

/// Every entry of the opened set gets a strip as long as its clip (the preview's frames).
#[test]
fn every_entry_strip_is_as_long_as_its_clip() {
    let set = procedural_locomotion();
    let (path, mut world) = opened("strips", &set.source);
    let runtime = set.build().expect("build");
    for range in runtime.clips() {
        let mut strip = Strip::new(&mut world, range.source);
        strip.frame(&mut world, Vec::new());
        assert_eq!(
            strip.geometry(&world).frames as usize,
            range.frames,
            "entry {}",
            range.source
        );
    }
    cleanup(&path);
}

/// A document entry with no loadable clip still gets a strip (as long as its spans) and a note.
#[test]
fn an_unloadable_clip_still_has_a_strip() {
    let set = procedural_locomotion();
    let mut source = set.source.clone();
    source.entries.push(EntrySource {
        clip: "anim/missing.clip".into(),
        looping: false,
        tags: Vec::new(),
        mirrored: false,
        trim: (0.0, 0.0),
        spans: vec![SpanSource {
            kind: SpanKindSource::Unindexed,
            range: (0.0, 2.0),
        }],
        distance_matched: false,
    });
    let (path, mut world) = opened("missing", &source);
    let index = source.entries.len() - 1;
    let strip = Strip::new(&mut world, index);
    let g = strip.geometry(&world);
    assert_eq!(g.frames, super::ops::time_frame(source.schema.sample_rate, 2.0));
    let err = super::preview::frame_view(&mut world, index, 0, false).expect_err("no clip");
    assert_eq!(err.code, "unknown_clip");
    cleanup(&path);
}
