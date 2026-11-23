// SPDX-License-Identifier: MPL-2.0
//! Spec 54 tests 10-15: marks, ranges, lanes, the `.regiatrace` file, the exports and the
//! rolling window. The recorder takes an injected clock; times are never compared to a budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_telemetry::trace::export::{export_jsonl, export_trace_event};
use regia_telemetry::trace::{
    Event, EventKind, Lane, RangeId, Recorder, RecorderState, Streams, TraceConfig, TraceError,
    TraceReadError, read_trace, read_trace_bytes,
};
use regia_test_utils::unique_temp_dir;

const SECOND: u64 = 1_000_000_000;

struct Clock(Arc<AtomicU64>);

impl Clock {
    fn tick(&self, ns: u64) {
        self.0.fetch_add(ns, Ordering::Relaxed);
    }
}

fn recorder(window_seconds: f32) -> (Recorder, Clock, std::path::PathBuf) {
    let now = Arc::new(AtomicU64::new(10 * SECOND));
    let dir = unique_temp_dir("trace");
    let probe = now.clone();
    let r = Recorder::with_clock(
        TraceConfig {
            window_seconds,
            dir: dir.clone(),
            ..TraceConfig::default()
        },
        Box::new(move || probe.load(Ordering::Relaxed)),
    );
    (r, Clock(now), dir)
}

fn kinds(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| match &e.kind {
            EventKind::Mark { name, .. } => format!("mark:{name}"),
            EventKind::RangeOpen { name, depth, .. } => format!("open:{name}@{depth}"),
            EventKind::RangeClose { forced, .. } => format!("close{}", if *forced { "!" } else { "" }),
            _ => "other".to_owned(),
        })
        .collect()
}

#[test]
fn marks_keep_order_and_the_rate_limit_drops_the_101st_per_second_per_name() {
    let (r, clock, _dir) = recorder(5.0);
    for name in ["a", "b", "c"] {
        clock.tick(10);
        assert!(r.mark(name, None).recorded);
    }
    let t: Vec<u64> = r.window_events().iter().map(|e| e.t_ns).collect();
    assert!(t.windows(2).all(|w| w[0] <= w[1]), "non-decreasing: {t:?}");
    assert_eq!(kinds(&r.window_events()), ["mark:a", "mark:b", "mark:c"]);

    let mut kept = 0;
    for _ in 0..101 {
        kept += u32::from(r.mark("burst", None).recorded);
    }
    assert_eq!(kept, 100);
    assert_eq!(r.status().dropped, 1, "counted");
    clock.tick(SECOND);
    assert!(r.mark("burst", None).recorded, "a new second starts a new budget");
}

#[test]
fn an_idle_recorder_keeps_nothing() {
    let (r, _clock, _dir) = recorder(0.0);
    assert_eq!(r.state(), RecorderState::Idle);
    assert!(!r.mark("x", None).recorded);
    assert_eq!(r.open_range("r", Lane::Main).id, RangeId::DROPPED);
    assert!(r.window_events().is_empty());
}

/// A non-finite window duration (NaN, ±inf) means "no window", like 0: an infinite horizon would
/// never expire events by time, so `+inf` must not leave the recorder `Windowed` after the clear.
#[test]
fn infinite_window_seconds_means_no_window() {
    let (r, clock, _dir) = recorder(5.0);
    assert_eq!(r.state(), RecorderState::Windowed);
    clock.tick(10);
    assert!(r.mark("before", None).recorded);
    r.set_window_seconds(f32::INFINITY);
    assert_eq!(r.state(), RecorderState::Idle, "a non-finite window is no window");
    assert!(r.window_events().is_empty(), "the window was cleared");
    clock.tick(10);
    assert!(!r.mark("after", None).recorded, "an idle recorder keeps nothing");
    assert!(r.window_events().is_empty());
}

#[test]
fn nested_ranges_report_depth_and_parent_and_stop_closes_open_ones() {
    let (r, clock, dir) = recorder(5.0);
    r.start(Some("nest"), None).expect("start");
    let a = r.open_range("A", Lane::Main);
    let b = r.open_range("B", Lane::Main);
    let c = r.open_range("C", Lane::Main);
    assert_eq!((a.depth, b.depth, c.depth), (0, 1, 2));
    clock.tick(500);
    // Closing A first force-closes C then B.
    let closed = r.close_range(a.id).expect("known");
    assert_eq!(closed.forced_close, [c.id, b.id]);
    assert_eq!(closed.duration_ns, 500);
    assert!(r.close_range(a.id).is_err(), "already closed");
    assert_eq!(r.status().dropped, 1, "the unknown id is counted");
    let open_at_stop = r.open_range("D", Lane::Main);
    assert!(open_at_stop.recorded);
    let summary = r.stop().expect("stop");
    assert_eq!(summary.state, RecorderState::Windowed);

    let trace = read_trace(&dir.join("nest.regiatrace")).expect("reads");
    assert_eq!(
        kinds(&trace.events),
        [
            "open:A@0", "open:B@1", "open:C@2", "close!", "close!", "close", "open:D@0", "close!"
        ]
    );
    let parents: Vec<Option<u64>> = trace
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::RangeOpen { parent, .. } => Some(parent),
            _ => None,
        })
        .collect();
    assert_eq!(parents[0], None);
    assert_eq!(parents[1], Some(a.id.0));
    assert_eq!(parents[2], Some(b.id.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_33rd_level_is_dropped_and_counted() {
    let (r, _clock, _dir) = recorder(5.0);
    let mut ids = Vec::new();
    for i in 0..32 {
        let o = r.open_range("deep", Lane::Main);
        assert!(o.recorded, "level {i}");
        ids.push(o.id);
    }
    let over = r.open_range("deep", Lane::Main);
    assert!(!over.recorded);
    assert_eq!(over.id, RangeId::DROPPED);
    assert_eq!(r.status().dropped, 1);
    assert_eq!(r.status().ranges_open, 32);
}

#[test]
fn ranges_on_two_lanes_nest_independently() {
    let (r, _clock, _dir) = recorder(5.0);
    let r = Arc::new(r);
    let a = r.open_range("main_outer", Lane::Main);
    let worker = {
        let r = r.clone();
        std::thread::spawn(move || {
            let w = r.open_range("worker_outer", Lane::Worker(1));
            let w2 = r.open_range("worker_inner", Lane::Worker(1));
            (w.depth, w2.depth, w2.id)
        })
        .join()
        .expect("worker")
    };
    assert_eq!((worker.0, worker.1), (0, 1));
    let main_inner = r.open_range("main_inner", Lane::Main);
    assert_eq!(main_inner.depth, 1);
    // Closing main's outer range does not touch the worker's stack.
    let closed = r.close_range(a.id).expect("known");
    assert_eq!(closed.forced_close, [main_inner.id]);
    assert_eq!(r.status().ranges_open, 2);
}

fn sample_events() -> Vec<Event> {
    let n: Arc<str> = Arc::from("gbuffer");
    let kinds = vec![
        (Lane::Main, EventKind::FrameBegin { frame: 1 }),
        (
            Lane::Main,
            EventKind::Mark {
                name: "boss door".into(),
                data: Some("{\"n\":1}".into()),
                frame: 1,
            },
        ),
        (
            Lane::Main,
            EventKind::RangeOpen {
                id: 7,
                name: "stress".into(),
                frame: 1,
                depth: 0,
                parent: None,
            },
        ),
        (
            Lane::Worker(3),
            EventKind::RangeOpen {
                id: 8,
                name: "job".into(),
                frame: 1,
                depth: 1,
                parent: Some(7),
            },
        ),
        (
            Lane::Gpu,
            EventKind::PassOpen {
                tag: 5,
                name: n.clone(),
                frame: 1,
                submission: 2,
            },
        ),
        (
            Lane::Gpu,
            EventKind::PassClose {
                tag: 5,
                name: n.clone(),
                frame: 1,
                submission: 2,
            },
        ),
        (
            Lane::Gpu,
            EventKind::GpuInterval {
                tag: 5,
                name: n,
                begin_ns: 10_000_000_100,
                end_ns: 10_000_000_900,
                frame: 1,
            },
        ),
        (
            Lane::Main,
            EventKind::Log {
                level: 2,
                target: "regia::x".into(),
                text: "hello".into(),
            },
        ),
        (
            Lane::Main,
            EventKind::Counter {
                name: "draws".into(),
                value: 1234.5,
            },
        ),
        (
            Lane::Main,
            EventKind::Screenshot {
                name: Some("after".into()),
                frame: 1,
                width: 640,
                height: 360,
                file: "t.regiatrace.shots/1.png".into(),
            },
        ),
        (
            Lane::Worker(3),
            EventKind::RangeClose {
                id: 8,
                frame: 1,
                forced: true,
            },
        ),
        (
            Lane::Main,
            EventKind::RangeClose {
                id: 7,
                frame: 1,
                forced: false,
            },
        ),
        (Lane::Main, EventKind::FrameEnd { frame: 1 }),
    ];
    kinds
        .into_iter()
        .enumerate()
        .map(|(i, (lane, kind))| Event {
            t_ns: 10_000_000_000 + i as u64 * 1_000,
            lane,
            kind,
        })
        .collect()
}

/// Encodes `events` the way the recorder's sink does, without a recorder (round trips every kind).
fn encode(events: &[Event]) -> Vec<u8> {
    use regia_telemetry::trace::events::Encoder;
    use regia_telemetry::trace::file::{Header, chunk, encode_chunk, encode_header};
    let mut out = encode_header(&Header {
        format_version: 1,
        start_unix_ms: 1,
        engine_version: "test".into(),
        ..Header::default()
    });
    let mut enc = Encoder::new();
    for e in events {
        enc.push(e, &mut out);
    }
    enc.cut(&mut out);
    out.extend(encode_chunk(chunk::INDEX, &[0, 0]));
    out
}

/// Field order on the wire, not only the decoded struct: Mark is name sid, data sid, frame;
/// RangeOpen is id, name sid, frame, depth, parent+1 (0 when there is no parent);
/// RangeClose is id, frame, forced. `docs/formats/regiatrace-v1.md`.
#[test]
fn mark_and_range_fields_are_in_wire_order() {
    use regia_telemetry::trace::file::{Cursor, chunk, decode_header, read_chunks};
    let events = [
        Event {
            t_ns: 1_000,
            lane: Lane::Main,
            kind: EventKind::Mark {
                name: "boss".into(),
                data: Some("door".into()),
                frame: 4,
            },
        },
        Event {
            t_ns: 1_100,
            lane: Lane::Main,
            kind: EventKind::RangeOpen {
                id: 7,
                name: "outer".into(),
                frame: 4,
                depth: 0,
                parent: None,
            },
        },
        Event {
            t_ns: 1_200,
            lane: Lane::Main,
            kind: EventKind::RangeOpen {
                id: 8,
                name: "inner".into(),
                frame: 4,
                depth: 1,
                parent: Some(7),
            },
        },
        Event {
            t_ns: 1_300,
            lane: Lane::Main,
            kind: EventKind::RangeClose {
                id: 8,
                frame: 5,
                forced: true,
            },
        },
    ];
    let bytes = encode(&events);
    let (_header, at) = decode_header(&bytes).expect("header");
    let (chunks, truncated) = read_chunks(&bytes, at);
    assert!(!truncated);
    let strings = chunks.iter().find(|c| c.kind == chunk::STRINGS).expect("strings");
    let mut cur = Cursor::new(&strings.payload);
    let mut names = std::collections::HashMap::<u64, String>::new();
    while !cur.done() {
        let id = cur.varint().expect("sid");
        let len = usize::try_from(cur.varint().expect("len")).expect("len");
        let text = String::from_utf8(cur.take(len).expect("bytes").to_vec()).expect("utf8");
        names.insert(id, text);
    }
    let sid = |text: &str| -> u64 {
        names
            .iter()
            .find_map(|(id, name)| (name == text).then_some(*id))
            .unwrap_or_else(|| panic!("missing string {text}"))
    };
    let events_chunk = chunks.iter().find(|c| c.kind == chunk::EVENTS).expect("events");
    let mut cur = Cursor::new(&events_chunk.payload);
    assert_eq!(cur.u8(), Some(1), "Mark");
    let _ = cur.varint().expect("t");
    let _ = cur.varint().expect("lane");
    assert_eq!(cur.varint(), Some(sid("boss")));
    assert_eq!(cur.varint(), Some(sid("door")));
    assert_eq!(cur.varint(), Some(4));
    assert_eq!(cur.u8(), Some(2), "RangeOpen");
    let _ = cur.varint().expect("t");
    let _ = cur.varint().expect("lane");
    assert_eq!(cur.varint(), Some(7));
    assert_eq!(cur.varint(), Some(sid("outer")));
    assert_eq!(cur.varint(), Some(4));
    assert_eq!(cur.varint(), Some(0));
    assert_eq!(cur.varint(), Some(0), "no parent is stored as 0");
    assert_eq!(cur.u8(), Some(2), "nested RangeOpen");
    let _ = cur.varint().expect("t");
    let _ = cur.varint().expect("lane");
    assert_eq!(cur.varint(), Some(8));
    assert_eq!(cur.varint(), Some(sid("inner")));
    assert_eq!(cur.varint(), Some(4));
    assert_eq!(cur.varint(), Some(1));
    assert_eq!(cur.varint(), Some(8), "parent 7 is stored as parent+1");
    assert_eq!(cur.u8(), Some(3), "RangeClose");
    let _ = cur.varint().expect("t");
    let _ = cur.varint().expect("lane");
    assert_eq!(cur.varint(), Some(8));
    assert_eq!(cur.varint(), Some(5));
    assert_eq!(cur.varint(), Some(1));
    assert!(cur.done(), "no trailing field");
}

#[test]
fn every_event_kind_round_trips() {
    let events = sample_events();
    let trace = read_trace_bytes(&encode(&events)).expect("reads");
    assert!(!trace.truncated);
    assert_eq!(trace.events, events);
    assert_eq!(trace.index, [(0, 0)]);
}

#[test]
fn a_truncated_file_reads_up_to_its_last_complete_chunk() {
    let events = sample_events();
    // Several chunks: one per screenshot/counter cut.
    let bytes = encode(&events);
    let full = read_trace_bytes(&bytes).expect("full");
    let mut seen_lengths = std::collections::BTreeSet::new();
    // Seeded offsets: every 7th byte past the header.
    let header_len = 4 + 4 + 4 + 8 + 4 + (2 + 4) + 2 + 2 + 2;
    assert_eq!(
        read_trace_bytes(&bytes[..header_len - 1]).expect_err("cut header"),
        TraceReadError::ShortHeader
    );
    for cut in (header_len..bytes.len()).step_by(7) {
        let part = read_trace_bytes(&bytes[..cut]).expect("a cut file still reads");
        assert!(part.events.len() <= full.events.len());
        assert_eq!(
            part.events,
            full.events[..part.events.len()],
            "a prefix of the full trace"
        );
        seen_lengths.insert(part.events.len());
    }
    assert!(
        seen_lengths.len() > 2,
        "cuts landed in different chunks: {seen_lengths:?}"
    );
    // A flipped payload byte stops the read at that chunk, not later.
    let mut bad = bytes.clone();
    let at = bad.len() / 2;
    bad[at] ^= 0xFF;
    let damaged = read_trace_bytes(&bad).expect("reads");
    assert!(damaged.truncated);
    assert!(damaged.events.len() < full.events.len());
}

#[test]
fn unknown_chunk_kinds_are_skipped_and_a_future_version_is_refused() {
    use regia_telemetry::trace::file::encode_chunk;
    let events = sample_events();
    let mut bytes = encode(&events);
    bytes.extend(encode_chunk(99, b"future data"));
    let trace = read_trace_bytes(&bytes).expect("reads");
    assert_eq!(trace.unknown_chunks, 1);
    assert_eq!(trace.events, events);

    let mut v2 = encode(&events);
    v2[4..8].copy_from_slice(&2u32.to_le_bytes());
    let err = read_trace_bytes(&v2).expect_err("refused");
    assert_eq!(err, TraceReadError::UnsupportedVersion(2));
    assert!(err.to_string().contains("newer than this build"), "{err}");
    assert_eq!(
        read_trace_bytes(b"nope").expect_err("bad"),
        TraceReadError::BadMagic
    );
}

#[test]
fn the_checked_in_v1_fixture_always_opens() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/v1.regiatrace");
    if std::env::var_os("REGIA_WRITE_TRACE_FIXTURE").is_some() {
        std::fs::write(&path, encode(&sample_events())).expect("write fixture");
    }
    let trace = read_trace(&path).expect("the v1 fixture opens");
    assert_eq!(trace.header.format_version, 1);
    assert_eq!(trace.header.engine_version, "test");
    assert_eq!(trace.events, sample_events());
}

#[test]
fn large_payloads_compress_and_still_read_back() {
    let events: Vec<Event> = (0..400)
        .map(|i| Event {
            t_ns: 10 * SECOND + i,
            lane: Lane::Main,
            kind: EventKind::Mark {
                name: "repeated mark name for compression".into(),
                data: None,
                frame: i / 10,
            },
        })
        .collect();
    let bytes = encode(&events);
    // Walk the chunks: the big events chunk must carry the LZ4 bit (0x80) and be smaller than
    // its decoded payload; a length cap alone passes even if compression never ran.
    let (_, mut at) = regia_telemetry::trace::file::decode_header(&bytes).expect("header");
    let mut compressed_events = 0;
    while at + 5 <= bytes.len() {
        let kind = bytes[at];
        let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().expect("4 bytes")) as usize;
        if kind == regia_telemetry::trace::file::chunk::EVENTS | 0x80 {
            compressed_events += 1;
            let decoded = u32::from_le_bytes(bytes[at + 5..at + 9].try_into().expect("size prefix")) as usize;
            assert!(len < decoded, "LZ4 shrank the events chunk: {len} vs {decoded}");
        }
        at += 5 + len + 4;
    }
    assert!(compressed_events >= 1, "an events chunk is LZ4-compressed");
    assert_eq!(read_trace_bytes(&bytes).expect("reads").events, events);
}

#[test]
fn exports_keep_count_names_and_matched_pairs() {
    let events = sample_events();
    let trace = read_trace_bytes(&encode(&events)).expect("reads");

    let mut lines = Vec::new();
    export_jsonl(&trace, &mut lines).expect("jsonl");
    let text = String::from_utf8(lines).expect("utf8");
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("line"))
        .collect();
    assert_eq!(rows.len(), events.len() + 1, "header + one per event");
    assert_eq!(rows[0]["format_version"], 1);
    assert!(
        rows.iter()
            .any(|r| r["type"] == "mark" && r["name"] == "boss door")
    );
    assert!(
        rows.iter()
            .any(|r| r["type"] == "gpu_interval" && r["name"] == "gbuffer")
    );
    let closes: Vec<_> = rows.iter().filter(|r| r["type"] == "range_close").collect();
    assert!(!closes.is_empty());
    assert!(
        closes
            .iter()
            .all(|r| r["forced"].is_boolean() && r.get("forced_close").is_none()),
        "the wire field is `forced` (a bool)"
    );

    let mut te = Vec::new();
    export_trace_event(&trace, &mut te).expect("trace event");
    let doc: serde_json::Value = serde_json::from_slice(&te).expect("parses");
    let evs = doc["traceEvents"].as_array().expect("array");
    let count = |ph: &str| evs.iter().filter(|e| e["ph"] == ph).count();
    assert_eq!(count("B"), count("E"), "matched B/E pairs");
    assert!(
        evs.iter()
            .filter(|e| e["ph"] == "E" && e["args"].is_object())
            .all(|e| e["args"]["forced"].is_boolean() && e["args"].get("forced_close").is_none())
    );
    assert!(count("B") >= 3);
    assert!(evs.iter().any(|e| e["ph"] == "i" && e["name"] == "boss door"));
    let x: Vec<_> = evs.iter().filter(|e| e["ph"] == "X").collect();
    assert_eq!(x.len(), 1);
    assert_eq!(x[0]["dur"].as_f64(), Some(0.8));
    let shot = evs
        .iter()
        .find(|e| e["args"]["file"].is_string())
        .expect("screenshot mark");
    assert_eq!(shot["args"]["file"], "t.regiatrace.shots/1.png");
}

#[test]
fn start_from_windowed_writes_the_window_first_and_save_window_leaves_the_file_alone() {
    let (r, clock, dir) = recorder(5.0);
    assert_eq!(r.state(), RecorderState::Windowed);
    r.mark("before the call", None);
    clock.tick(SECOND);
    let started = r.start(Some("run"), None).expect("start");
    assert_eq!(started.state, RecorderState::Writing);
    assert_eq!(r.start(Some("other"), None), Err(TraceError::Busy));
    clock.tick(SECOND);
    r.mark("after the call", None);

    let saved = r.save_window(Some("snap")).expect("save window");
    assert_eq!(saved.file.as_deref(), Some(dir.join("snap.regiatrace").as_path()));
    assert_eq!(r.state(), RecorderState::Writing, "still writing");
    let snap = read_trace(&dir.join("snap.regiatrace")).expect("window file");
    assert_eq!(
        kinds(&snap.events),
        ["mark:before the call", "mark:after the call"]
    );

    let done = r.stop().expect("stop");
    assert_eq!(done.events, 2);
    let run = read_trace(&dir.join("run.regiatrace")).expect("run file");
    assert_eq!(
        kinds(&run.events),
        ["mark:before the call", "mark:after the call"],
        "the file starts before the start call"
    );
    assert!(run.events[0].t_ns < run.events[1].t_ns);
    assert_eq!(r.stop(), Err(TraceError::NotWriting));

    // The window evicts by age.
    clock.tick(10 * SECOND);
    r.mark("late", None);
    assert_eq!(kinds(&r.window_events()), ["mark:late"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_names_must_be_bare() {
    let (r, _clock, dir) = recorder(5.0);
    for bad in ["../x", "a/b", "a\\b", "", ".hidden", "x\0y", "C:x", "a:b"] {
        assert_eq!(r.start(Some(bad), None), Err(TraceError::BadFileName), "{bad:?}");
    }
    assert_eq!(TraceError::BadFileName.reason(), "bad_file_name");
    assert!(
        dir.read_dir().expect("trace dir exists").next().is_none(),
        "nothing was created"
    );
}

#[test]
fn streams_select_what_is_recorded() {
    let (r, _clock, dir) = recorder(5.0);
    assert!(r.wants("frames") && r.wants("passes") && !r.wants("counters"));
    let streams = Streams::from_names(["counters"]).expect("known");
    r.start(Some("s"), Some(streams)).expect("start");
    assert!(r.wants("counters") && !r.wants("frames"));
    r.counter("draws", 3.0);
    r.frame_begin(4);
    let done = r.stop().expect("stop");
    assert_eq!(done.events, 1);
    assert!(Streams::from_names(["nope"]).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn breadcrumb_passes_become_gpu_lane_events_once() {
    use regia_render::breadcrumbs::{Breadcrumbs, CrumbList};
    let b = Breadcrumbs::default();
    let mut list = CrumbList::new();
    drop(b.pass(&mut list, "shadow_cascades"));
    drop(b.pass(&mut list, "opaque"));
    b.note_submit(&[&list], 4);
    let (r, _clock, _dir) = recorder(5.0);
    r.record_breadcrumbs(&b.snapshot());
    r.record_breadcrumbs(&b.snapshot());
    let ev = r.window_events();
    assert_eq!(ev.len(), 4, "two passes, open and close, recorded once");
    assert!(ev.iter().all(|e| e.lane == Lane::Gpu));
    match &ev[2].kind {
        EventKind::PassOpen { name, frame, .. } => assert_eq!((&**name, *frame), ("opaque", 4)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn breadcrumb_range_with_start_after_end_is_skipped() {
    use regia_render::breadcrumbs::{Breadcrumbs, CrumbList};
    let b = Breadcrumbs::default();
    let mut list = CrumbList::new();
    drop(b.pass(&mut list, "opaque"));
    b.note_submit(&[&list], 4);
    let mut snap = b.snapshot();
    for f in &mut snap.frames {
        for sub in &mut f.submissions {
            std::mem::swap(&mut sub.start, &mut sub.end);
            sub.start = sub.start.max(1);
            sub.end = 0;
        }
    }
    let (r, _clock, _dir) = recorder(5.0);
    r.record_breadcrumbs(&snap);
    assert!(r.window_events().is_empty());
}

#[test]
fn a_screenshot_mark_is_stored_relative_to_the_trace() {
    let (r, _clock, dir) = recorder(5.0);
    r.start(Some("shots"), None).expect("start");
    let shots = r.shots_dir().expect("dir while writing");
    assert!(shots.ends_with("shots.regiatrace.shots"));
    let png = shots.join("1.png");
    std::fs::write(&png, b"png").expect("write");
    let m = r.screenshot_mark(Some("hit"), 12, (64, 32), &png);
    assert!(m.recorded);
    r.stop().expect("stop");
    let trace = read_trace(&dir.join("shots.regiatrace")).expect("reads");
    match &trace.events[0].kind {
        EventKind::Screenshot {
            file, frame, width, ..
        } => {
            assert_eq!(file, "shots.regiatrace.shots/1.png");
            assert_eq!((*frame, *width), (12, 64));
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_second_index_offsets_are_distinct_for_small_traces() {
    let (r, clock, dir) = recorder(0.0);
    r.start(Some("idx"), None).expect("start");
    for _ in 0..3 {
        r.mark("tick", None);
        clock.tick(SECOND);
    }
    r.stop().expect("stop");
    let run = read_trace(&dir.join("idx.regiatrace")).expect("run file");
    assert_eq!(run.index.len(), 3, "{:?}", run.index);
    assert!(
        run.index.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1),
        "each second starts at its own chunk: {:?}",
        run.index
    );
}

/// Captures `regia::mark` tracing events as `(name, frame)`.
#[derive(Clone, Default)]
struct MarkCapture(Arc<std::sync::Mutex<Vec<(String, u64)>>>);

#[derive(Default)]
struct MarkFields {
    name: String,
    frame: Option<u64>,
}

impl tracing::field::Visit for MarkFields {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "frame" {
            self.frame = Some(value);
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "name" => self.name = format!("{value:?}"),
            "frame" if self.frame.is_none() => self.frame = format!("{value:?}").parse().ok(),
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MarkCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "regia::mark" {
            return;
        }
        let mut fields = MarkFields::default();
        event.record(&mut fields);
        self.0
            .lock()
            .expect("capture lock")
            .push((fields.name, fields.frame.unwrap_or(u64::MAX)));
    }
}

/// Spec 54 § 2.4 and test 10, last clause: each mark also emits a `regia::mark` tracing event
/// with its name and frame, so a host with the JSON log layer sees it without a trace file. It
/// is emitted whether or not the mark is stored (here the third one is over the rate limit).
#[test]
fn each_mark_also_emits_a_regia_mark_tracing_event() {
    use tracing_subscriber::layer::SubscriberExt;
    let capture = MarkCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let (r, clock, _dir) = recorder(5.0);
    tracing::subscriber::with_default(subscriber, || {
        r.set_frame(7);
        assert!(r.mark("boss_door", None).recorded);
        clock.tick(10);
        r.set_frame(8);
        assert!(
            r.mark("foliage_stress", Some(&serde_json::json!({ "n": 1 })))
                .recorded
        );
        for _ in 0..100 {
            r.mark("burst", None);
        }
        assert!(
            !r.mark("burst", None).recorded,
            "the 101st in a second is dropped"
        );
    });
    let seen = capture.0.lock().expect("capture lock").clone();
    assert_eq!(
        seen.len(),
        103,
        "one event per mark call, stored or not: {seen:?}"
    );
    assert_eq!(seen[0], ("boss_door".to_owned(), 7));
    assert_eq!(seen[1], ("foliage_stress".to_owned(), 8));
    assert!(
        seen[2..].iter().all(|(n, f)| n == "burst" && *f == 8),
        "{:?}",
        &seen[2..]
    );
}
