// SPDX-License-Identifier: MPL-2.0
//! Spec 54 tests 1-8: the breadcrumb ring, counters and report, headless (no device). Every test
//! builds its own `Breadcrumbs`, so none touches the process-wide writer.

use regia_render::breadcrumbs::{
    BreadcrumbConfig, BreadcrumbReport, Breadcrumbs, CrumbList, EvidenceLevel, GpuProgressSource,
    MAX_MARKERS, OVERFLOW_NAME, ScriptedProgress, SubmissionId, TimestampEvidence,
};

const PASSES: [&str; 10] = [
    "shadow_cascades",
    "depth_prepass",
    "gbuffer",
    "ssao",
    "lighting",
    "opaque",
    "transparent",
    "post",
    "tonemap",
    "ui",
];

fn frame_list(b: &Breadcrumbs, names: &[&'static str]) -> CrumbList {
    let mut list = CrumbList::new();
    for name in names {
        drop(b.pass(&mut list, name));
    }
    list
}

fn submit_frame(b: &Breadcrumbs, frame: u64) -> SubmissionId {
    let list = frame_list(b, &PASSES);
    b.note_submit(&[&list], frame)
}

#[test]
fn ring_returns_frames_newest_first_with_paired_names() {
    let b = Breadcrumbs::default();
    for frame in 10..13 {
        submit_frame(&b, frame);
    }
    let snap = b.snapshot();
    assert_eq!(
        snap.frames.iter().map(|f| f.frame).collect::<Vec<_>>(),
        [12, 11, 10]
    );
    let report = BreadcrumbReport::capture(&snap, None);
    assert_eq!(report.frames[0].passes, PASSES);
    for f in &snap.frames {
        assert_eq!(f.markers.len(), 20);
        // Open and close pair up: the markers alternate open, close for sequential scopes.
        assert!(
            f.markers
                .chunks(2)
                .all(|c| c[0].open && !c[1].open && c[0].tag == c[1].tag)
        );
    }
    assert_eq!(snap.counters.submitted, 3);
}

#[test]
fn nested_scopes_close_in_reverse_order() {
    let b = Breadcrumbs::default();
    let mut list = CrumbList::new();
    {
        let _outer = b.pass(&mut list, "outer");
    }
    drop(b.pass(&mut list, "inner"));
    let tags: Vec<_> = list.markers().map(|m| (b.name_of(m.tag), m.open)).collect();
    assert_eq!(
        tags,
        [
            ("outer".to_owned(), true),
            ("outer".to_owned(), false),
            ("inner".to_owned(), true),
            ("inner".to_owned(), false)
        ]
    );
}

#[test]
fn ring_wraps_to_the_newest_frames() {
    let b = Breadcrumbs::new(BreadcrumbConfig {
        ring_frames: 4,
        ..BreadcrumbConfig::default()
    });
    for frame in 0..(4 + 3) {
        submit_frame(&b, frame);
    }
    let frames: Vec<u64> = b.snapshot().frames.iter().map(|f| f.frame).collect();
    assert_eq!(frames, [6, 5, 4, 3]);
}

#[test]
fn ring_frames_are_clamped() {
    let b = Breadcrumbs::new(BreadcrumbConfig {
        ring_frames: 0,
        ..BreadcrumbConfig::default()
    });
    for frame in 0..5 {
        submit_frame(&b, frame);
    }
    assert_eq!(b.snapshot().frames.len(), 2);
}

#[test]
fn overflow_keeps_the_oldest_markers_and_the_next_frame_starts_clean() {
    let b = Breadcrumbs::default();
    let mut list = CrumbList::new();
    for _ in 0..(MAX_MARKERS) {
        drop(b.pass(&mut list, "many"));
    }
    assert_eq!(list.len(), MAX_MARKERS);
    b.note_submit(&[&list], 1);
    let snap = b.snapshot();
    assert!(snap.frames[0].truncated);
    assert_eq!(snap.frames[0].markers.len(), MAX_MARKERS);
    assert!(snap.frames[0].markers[0].open, "the oldest marker survives");
    // A second list in the same frame cannot add past the cap either.
    let more = frame_list(&b, &["late"]);
    b.note_submit(&[&more], 1);
    assert_eq!(b.snapshot().frames[0].markers.len(), MAX_MARKERS);
    submit_frame(&b, 2);
    let snap = b.snapshot();
    assert_eq!(snap.frames[0].frame, 2);
    assert!(!snap.frames[0].truncated);
    assert_eq!(snap.frames[0].markers.len(), 20);
}

#[test]
fn name_table_overflows_to_tag_zero() {
    let b = Breadcrumbs::default();
    let first = b.intern("a_pass");
    assert_eq!(b.intern("a_pass"), first, "interning is stable");
    for i in 0..regia_render::breadcrumbs::MAX_NAMES + 5 {
        let _ = b.intern(&format!("pass_{i}"));
    }
    let late = b.intern("one_too_many");
    assert_eq!(b.name_of(late), OVERFLOW_NAME);
    assert_eq!(b.name_of(first), "a_pass", "earlier tags stay valid");
}

#[test]
fn submission_order_is_buffer_order_not_recording_order() {
    let b = Breadcrumbs::default();
    // Recorded in reverse (as two workers might finish), submitted as [a, b].
    let second = frame_list(&b, &["b_pass"]);
    let first = frame_list(&b, &["a_pass"]);
    b.note_submit(&[&first, &second], 7);
    let report = BreadcrumbReport::capture(&b.snapshot(), None);
    assert_eq!(report.frames[0].passes, ["a_pass", "b_pass"]);
    // Graph node ids interned at compile resolve to names.
    let tag = b.intern("graph_node_17");
    assert_eq!(b.name_of(tag), "graph_node_17");
}

/// Spec 54 test 4 on real threads: two encode workers each fill their own segment list (no
/// shared write target), the second finishes first, and the frame thread submits `[a, b]`; the
/// record is in buffer order, each scope's open and close kept together.
#[test]
fn segments_recorded_on_two_workers_keep_submission_order() {
    let b = std::sync::Arc::new(Breadcrumbs::default());
    let (to_a, wait_a) = std::sync::mpsc::channel::<()>();
    let worker = |names: &'static [&'static str], gate: Option<std::sync::mpsc::Receiver<()>>| {
        let b = b.clone();
        std::thread::spawn(move || {
            if let Some(gate) = gate {
                gate.recv().expect("b finished first");
            }
            frame_list(&b, names)
        })
    };
    let a = worker(&["a_shadow", "a_opaque"], Some(wait_a));
    let second = worker(&["b_post", "b_ui"], None).join().expect("worker b");
    to_a.send(()).expect("start a");
    let first = a.join().expect("worker a");
    b.note_submit(&[&first, &second], 3);
    let report = BreadcrumbReport::capture(&b.snapshot(), None);
    assert_eq!(report.frames[0].frame, 3);
    assert_eq!(
        report.frames[0].passes,
        ["a_shadow", "a_opaque", "b_post", "b_ui"],
        "buffer order, not the order the workers finished"
    );
}

#[test]
fn counters_track_the_contiguous_completed_prefix() {
    let b = Breadcrumbs::default();
    for frame in 1..=5 {
        submit_frame(&b, frame);
    }
    assert_eq!(b.counters().submitted, 5);
    b.note_completed(SubmissionId(1));
    b.note_completed(SubmissionId(2));
    b.note_completed(SubmissionId(4));
    assert_eq!(b.counters().completed, 2, "3 is late");
    assert_eq!(b.counters().acknowledged_frame, 2);
    b.note_completed(SubmissionId(2));
    b.note_completed(SubmissionId(1));
    assert_eq!(b.counters().completed, 2, "duplicates are ignored");
    b.note_completed(SubmissionId(3));
    assert_eq!(b.counters().completed, 4);
    assert_eq!(b.counters().acknowledged_frame, 4);
    assert_eq!(b.counters().in_flight(), 1);
}

#[test]
fn counters_survive_a_gap_wider_than_the_window() {
    let b = Breadcrumbs::default();
    for frame in 1..=200 {
        submit_frame(&b, frame);
    }
    b.note_completed(SubmissionId(150));
    // Everything older than the 64-entry window is taken as completed.
    assert_eq!(b.counters().completed, 86);
    for id in 87..150 {
        b.note_completed(SubmissionId(id));
    }
    assert_eq!(b.counters().completed, 150);
}

#[test]
fn resume_barrier_forgives_lost_callbacks() {
    let b = Breadcrumbs::default();
    for frame in 1..=3 {
        submit_frame(&b, frame);
    }
    b.note_completed(SubmissionId(1));
    // Suspended with 2 and 3 in flight; their callbacks never arrive.
    b.note_resumed();
    submit_frame(&b, 4);
    b.note_completed(SubmissionId(4));
    assert_eq!(b.counters().completed, 4);
    assert_eq!(b.counters().in_flight(), 0);
}

#[test]
fn stall_then_loss_is_fenced_with_the_in_flight_passes_as_suspects() {
    let b = Breadcrumbs::default();
    let p = ScriptedProgress::new();
    p.stall_at_submission(3);
    for frame in 1..=4u64 {
        let id = submit_frame(&b, frame);
        if p_complete(&p, id) {
            b.note_completed(id);
        }
    }
    p.script_loss("unknown", "gpu hung");
    let (reason, message) = p.take_loss().expect("scripted loss");
    let info = regia_render::gpu_health::DeviceLostInfo { reason, message };
    let report = BreadcrumbReport::capture(&b.snapshot(), Some(&info));
    assert_eq!(report.level, EvidenceLevel::Fenced);
    assert_eq!(report.counters.completed, 2);
    assert_eq!(report.counters.submitted, 4);
    // Submissions 3 and 4, each of the ten passes, oldest first.
    assert_eq!(report.suspects.len(), 20);
    assert_eq!(report.suspects[0].submission, 3);
    assert_eq!(report.suspects[0].name, PASSES[0]);
    assert_eq!(report.suspects[10].submission, 4);
    assert_eq!(report.lost.as_ref().map(|l| l.message.as_str()), Some("gpu hung"));
}

/// Whether the scripted source completes `id` (it holds the callback of a stalled one).
fn p_complete(p: &ScriptedProgress, id: SubmissionId) -> bool {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    p.on_work_done(
        id,
        Box::new(move || flag.store(true, std::sync::atomic::Ordering::Relaxed)),
    );
    done.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn timestamps_ending_at_a_pass_give_the_next_pass_as_first_suspect() {
    let b = Breadcrumbs::default();
    for frame in 1..=2 {
        let id = submit_frame(&b, frame);
        b.note_completed(id);
    }
    submit_frame(&b, 3);
    // Passes 0..=4 of frame 3 resolved; the sixth is first among the suspects.
    b.note_timestamps(TimestampEvidence {
        frame: 3,
        resolved_passes: 5,
    });
    let report = BreadcrumbReport::capture(&b.snapshot(), None);
    assert_eq!(report.level, EvidenceLevel::Timestamped);
    assert_eq!(report.suspects[0].name, PASSES[5]);
    assert_eq!(report.suspects.len(), 5);
}

#[test]
fn a_disabled_writer_reports_level_none() {
    let b = Breadcrumbs::new(BreadcrumbConfig {
        enabled: false,
        ..BreadcrumbConfig::default()
    });
    let list = frame_list(&b, &PASSES);
    assert!(list.is_empty(), "off writes no marker");
    b.note_submit(&[&list], 1);
    let report = BreadcrumbReport::capture(&b.snapshot(), None);
    assert_eq!(report.level, EvidenceLevel::None);
    assert!(report.suspects.is_empty());
}

#[test]
fn little_information_gives_recorded_and_an_empty_message() {
    let b = Breadcrumbs::default();
    submit_frame(&b, 1);
    let info = regia_render::gpu_health::DeviceLostInfo {
        reason: "unknown",
        message: String::new(),
    };
    let report = BreadcrumbReport::capture(&b.snapshot(), Some(&info));
    assert_eq!(report.level, EvidenceLevel::Recorded);
    assert_eq!(report.lost.as_ref().map(|l| l.message.as_str()), Some(""));
    assert_eq!(report.suspects.len(), 10);
    // With completions seen but nothing in flight, the level is fenced and there are no suspects.
    b.note_completed(SubmissionId(1));
    let report = BreadcrumbReport::capture(&b.snapshot(), None);
    assert_eq!(report.level, EvidenceLevel::Fenced);
    assert!(report.suspects.is_empty());
}

#[test]
fn report_fits_64_kib_and_marks_truncation() {
    let b = Breadcrumbs::new(BreadcrumbConfig {
        ring_frames: 64,
        ..BreadcrumbConfig::default()
    });
    let names: Vec<&'static str> = (0..150)
        .map(|i| &*Box::leak(format!("a_rather_long_pass_name_{i}").into_boxed_str()))
        .collect();
    for frame in 0..64 {
        let list = frame_list(&b, &names);
        b.note_submit(&[&list], frame);
    }
    let report = BreadcrumbReport::capture(&b.snapshot(), None);
    let json = serde_json::to_vec(&report).expect("serializes");
    assert!(
        json.len() <= regia_render::breadcrumbs::REPORT_MAX_BYTES,
        "{} bytes",
        json.len()
    );
    assert!(report.truncated);
    assert_eq!(report.frames[0].frame, 63, "newest frames are kept");
    let back: BreadcrumbReport = serde_json::from_slice(&json).expect("round trips");
    assert_eq!(back.format_version, 1);
}

#[test]
fn crash_text_names_level_suspects_and_frames() {
    let b = Breadcrumbs::default();
    submit_frame(&b, 9);
    let text = BreadcrumbReport::capture(&b.snapshot(), None).crash_text();
    assert!(text.starts_with("gpu breadcrumbs: level 1 (recorded)"), "{text}");
    assert!(
        text.contains("frame 9: shadow_cascades > depth_prepass"),
        "{text}"
    );
}
