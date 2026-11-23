// SPDX-License-Identifier: MPL-2.0
//! ADR-2307: the process recorder follows the renderer's frame through the breadcrumb frame
//! observer (no system in the app's schedule): `begin_frame` ends the previous frame, begins the
//! new one, and records the breadcrumb passes submitted since, each once, on the `gpu` lane.
//!
//! Its own test binary: the recorder and the breadcrumb writer are process-wide. The counting
//! allocator is here so a pass trace can be checked for per-frame heap traffic (audit
//! sprint-audit-g-telemetry-perf, findings 1 and 2).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{Arc, Mutex, PoisonError};

use regia_render::breadcrumbs::{self, CrumbList};
use regia_telemetry::trace::events::Encoder;
use regia_telemetry::trace::{self, Event, EventKind, Lane, RecorderState, Streams, TraceConfig};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator and only counts calls on this thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

fn process_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn stop_if_writing() {
    let r = trace::recorder();
    if r.state() == RecorderState::Writing
        && let Err(e) = r.stop()
    {
        panic!("stop before the next recording: {e}");
    }
}

#[test]
fn begin_frame_drives_frames_and_passes_of_the_process_recorder() {
    let _lock = process_lock();
    let dir = regia_test_utils::unique_temp_dir("trace-observer");
    stop_if_writing();
    trace::configure(TraceConfig {
        dir: dir.clone(),
        ..TraceConfig::default()
    });
    let r = trace::recorder();
    if let Err(e) = r.start(Some("observed"), None) {
        panic!("start: {e}");
    }
    for frame in 1..=3u64 {
        breadcrumbs::begin_frame(frame);
        let mut list = CrumbList::new();
        drop(breadcrumbs::global().pass(&mut list, "observer_pass"));
        breadcrumbs::note_submit(&[&list], frame);
    }
    breadcrumbs::begin_frame(4);
    assert_eq!(r.frame(), 4, "marks carry the renderer's frame");
    if let Err(e) = r.stop() {
        panic!("stop: {e}");
    }
    let read = trace::read_trace(&dir.join("observed.regiatrace")).unwrap_or_else(|e| panic!("reads: {e}"));
    assert!(!read.events.is_empty());
    let begins: Vec<u64> = read
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::FrameBegin { frame } => Some(frame),
            _ => None,
        })
        .collect();
    let ends: Vec<u64> = read
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::FrameEnd { frame } => Some(frame),
            _ => None,
        })
        .collect();
    assert_eq!(begins, [1, 2, 3, 4]);
    assert_eq!(ends, [1, 2, 3]);
    let opens: Vec<(u64, Lane)> = read
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::PassOpen { name, frame, .. } if &**name == "observer_pass" => Some((*frame, e.lane)),
            _ => None,
        })
        .collect();
    assert_eq!(
        opens,
        [(1, Lane::Gpu), (2, Lane::Gpu), (3, Lane::Gpu)],
        "each submitted pass once, on the gpu lane"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 1: after the ring and the pass-name table are full, a pass trace's frame observer
/// must not clone either of them again on a steady frame.
#[test]
fn steady_pass_capture_does_not_clone_the_ring_or_the_name_table() {
    let _lock = process_lock();
    let dir = regia_test_utils::unique_temp_dir("trace-observer-alloc");
    stop_if_writing();
    trace::configure(TraceConfig {
        dir: dir.clone(),
        window_seconds: 0.0,
        streams: Streams {
            marks: true,
            frames: false,
            passes: true,
            counters: false,
            logs: false,
        },
        ..TraceConfig::default()
    });
    // Install the observer while the recorder is idle, then fill the ring and the name table.
    // Those copies are the steady-frame cost this test bounds.
    let _ = trace::recorder();
    const PAD_NAMES: usize = 300;
    for i in 0..PAD_NAMES {
        breadcrumbs::intern(&format!("pad{i}"));
    }
    for frame in 1..=8u64 {
        let mut list = CrumbList::new();
        for _ in 0..64 {
            drop(breadcrumbs::global().pass(&mut list, "bulk"));
        }
        breadcrumbs::note_submit(&[&list], frame);
    }
    let names = breadcrumbs::snapshot().names.len();
    assert!(
        names >= PAD_NAMES,
        "the name table should hold the padded names, has {names}"
    );

    let r = trace::recorder();
    if let Err(e) = r.start(Some("alloc"), None) {
        panic!("start: {e}");
    }
    let mut list = CrumbList::new();
    let mut frame = 10_000u64;
    // Catch the backlog up, then warm the steady pass so its name and the encoder buffer exist.
    breadcrumbs::begin_frame(frame);
    const WARM: usize = 32;
    for _ in 0..WARM {
        frame += 1;
        list.clear();
        drop(breadcrumbs::global().pass(&mut list, "steady"));
        breadcrumbs::note_submit(&[&list], frame);
        breadcrumbs::begin_frame(frame + 1);
    }

    const FRAMES: usize = 200;
    let mut allocs = 0usize;
    for _ in 0..FRAMES {
        frame += 1;
        list.clear();
        drop(breadcrumbs::global().pass(&mut list, "steady"));
        breadcrumbs::note_submit(&[&list], frame);
        let before = allocations();
        // Records the submission above. The ring still holds older frames and every interned name.
        breadcrumbs::begin_frame(frame + 1);
        allocs += allocations() - before;
    }
    let per_frame = allocs as f64 / FRAMES as f64;
    println!(
        "steady pass capture: {allocs} allocations over {FRAMES} frames ({per_frame:.2} per frame), name table {names}"
    );

    if let Err(e) = r.stop() {
        panic!("stop: {e}");
    }
    let read = trace::read_trace(&dir.join("alloc.regiatrace")).unwrap_or_else(|e| panic!("reads: {e}"));
    let steady = read
        .events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                EventKind::PassOpen { name, .. } if &**name == "steady"
            )
        })
        .count();
    assert_eq!(steady, WARM + FRAMES, "each steady submission is recorded once");

    // A per-frame name-table clone is `names` allocations (one per string) before the ring copy.
    // The bound sits under that clone and above the two short Vecs finding 2 still builds per
    // pass event, so this fails on the ring copy and passes once that copy is gone.
    let name_clone = names.saturating_mul(FRAMES);
    assert!(
        allocs.saturating_mul(4) < name_clone,
        "steady capture allocated {allocs} times over {FRAMES} frames ({per_frame:.1} per frame); cloning {names} pass names each frame would be {name_clone}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 2: encoding a pass event must not build two short Vecs under the recorder.
#[test]
fn pass_open_encode_does_not_allocate_two_vecs_per_event() {
    let name: Arc<str> = Arc::from("shadow");
    let event = Event {
        t_ns: 1_000,
        lane: Lane::Gpu,
        kind: EventKind::PassOpen {
            tag: 7,
            name,
            frame: 3,
            submission: 9,
        },
    };
    let mut enc = Encoder::new();
    let mut out = Vec::new();
    for _ in 0..20_000 {
        enc.push(&event, &mut out);
    }
    enc.cut(&mut out);
    out.reserve(1 << 20);
    // Stay under the encoder's 64 KiB event chunk so the window measures per-event traffic, not a flush.
    const EVENTS: usize = 1_000;
    let before = allocations();
    for _ in 0..EVENTS {
        enc.push(&event, &mut out);
    }
    let delta = allocations() - before;
    println!("pass encode: {delta} allocations for {EVENTS} identical PassOpen events after warm-up");
    assert!(
        delta < 8,
        "encoder allocated {delta} times for {EVENTS} identical PassOpen events after warm-up (two Vecs per event would be {})",
        EVENTS * 2
    );
}

#[derive(Clone, Default)]
struct SpanNames(Arc<Mutex<Vec<&'static str>>>);

impl<S: tracing::Subscriber> Layer<S> for SpanNames {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(attrs.metadata().name());
    }
}

/// Finding 3 (low): the per-frame pass capture enters one debug span the recommended filter keeps.
#[test]
fn pass_capture_enters_a_debug_span() {
    let _lock = process_lock();
    let dir = regia_test_utils::unique_temp_dir("trace-observer-span");
    stop_if_writing();
    trace::configure(TraceConfig {
        dir: dir.clone(),
        window_seconds: 0.0,
        ..TraceConfig::default()
    });
    let r = trace::recorder();
    if let Err(e) = r.start(Some("span"), None) {
        panic!("start: {e}");
    }
    breadcrumbs::begin_frame(1);
    let mut list = CrumbList::new();
    drop(breadcrumbs::global().pass(&mut list, "span_pass"));
    breadcrumbs::note_submit(&[&list], 1);

    let names = SpanNames::default();
    let filter = tracing_subscriber::EnvFilter::try_new(regia_telemetry::RECOMMENDED_FILTER)
        .unwrap_or_else(|e| panic!("filter: {e}"));
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(filter).with(names.clone()),
        || breadcrumbs::begin_frame(2),
    );
    let got = names.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(
        got.contains(&"trace_capture"),
        "a pass trace's begin_frame should enter trace_capture at debug on target regia, saw {got:?}"
    );
    if let Err(e) = r.stop() {
        panic!("stop: {e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
