// SPDX-License-Identifier: MPL-2.0
//! Logging pieces at their edges: the UTC calendar conversion against an independent day-by-day
//! calendar, the session file sink's naming and pruning, and how the in-memory ring formats events.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// The log ring is process-global. Tests that push into it take this lock so one test's records
/// do not land between another's.
static RING_EDGES: Mutex<()> = Mutex::new(());

use regia_core::log::ring::{self, CAPACITY, LogQuery, RingLayer};
use regia_core::log::{FileSink, civil_date_time};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-core-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

#[test]
fn civil_date_time_matches_a_day_by_day_calendar_for_four_centuries() {
    // 1970-01-01, then step one day at a time through 2370 (covers 2000, a 400-year leap, and 2100, a 100-year non-leap).
    let (mut y, mut m, mut d) = (1970i64, 1u32, 1u32);
    for day in 0u64..146_097 {
        let got = civil_date_time(day * 86_400);
        assert_eq!(got, (y, m, d, 0, 0, 0), "day {day}");
        // The last second of the day is the same date.
        assert_eq!(
            civil_date_time(day * 86_400 + 86_399),
            (y, m, d, 23, 59, 59),
            "day {day} end"
        );
        d += 1;
        if d > days_in_month(y, m) {
            d = 1;
            m += 1;
            if m > 12 {
                m = 1;
                y += 1;
            }
        }
    }
}

#[test]
fn civil_date_time_splits_the_time_of_day_and_survives_the_largest_input() {
    assert_eq!(civil_date_time(3_661), (1970, 1, 1, 1, 1, 1));
    assert_eq!(civil_date_time(86_399), (1970, 1, 1, 23, 59, 59));
    assert_eq!(civil_date_time(86_400), (1970, 1, 2, 0, 0, 0));
    let (y, m, d, h, mi, s) = civil_date_time(u64::MAX);
    assert!(y > 500_000_000_000, "{y}");
    assert!((1..=12).contains(&m) && (1..=31).contains(&d) && h < 24 && mi < 60 && s < 60);
}

#[test]
fn file_sink_zero_max_files_keeps_only_the_new_session() {
    let dir = temp_dir("sink0");
    fs::create_dir_all(&dir).expect("dir");
    for n in 1..=3 {
        fs::File::create(dir.join(format!("2020-01-0{n}_00-00-00.log"))).expect("old log");
    }
    let sink = FileSink::create(&dir, 0).expect("sink");
    let logs: Vec<_> = fs::read_dir(&dir).expect("read").filter_map(Result::ok).collect();
    assert_eq!(logs.len(), 1, "max_files 0 is treated as 1");
    assert_eq!(logs[0].path(), sink.path());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn file_sink_sessions_started_in_the_same_second_get_distinct_files() {
    let dir = temp_dir("sinkcollide");
    let sinks: Vec<_> = (0..4)
        .map(|_| FileSink::create(&dir, 10).expect("sink"))
        .collect();
    let mut paths: Vec<_> = sinks.iter().map(|s| s.path().to_path_buf()).collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 4, "no session overwrites another");
    // Each writes to its own file.
    for (i, sink) in sinks.iter().enumerate() {
        sink.make_writer()
            .write_all(format!("session {i}\n").as_bytes())
            .expect("write");
        sink.flush();
        assert_eq!(
            fs::read_to_string(sink.path()).expect("read"),
            format!("session {i}\n")
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn file_sink_cannot_be_created_below_a_regular_file() {
    let dir = temp_dir("sinkbad");
    fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("not_a_dir");
    fs::write(&file, b"x").expect("file");
    assert!(
        FileSink::create(&file.join("logs"), 5).is_err(),
        "an IO error, not a panic"
    );
    assert!(FileSink::create(&file, 5).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn file_sink_clones_share_one_file_and_debug_shows_the_path() {
    let dir = temp_dir("sinkclone");
    let a = FileSink::create(&dir, 5).expect("sink");
    let b = a.clone();
    a.make_writer().write_all(b"a;").expect("write");
    b.make_writer().write_all(b"b;").expect("write");
    b.flush();
    assert_eq!(a.path(), b.path());
    assert_eq!(fs::read_to_string(a.path()).expect("read"), "a;b;");
    assert!(format!("{a:?}").contains(".log"));
    let _ = fs::remove_dir_all(&dir);
}

/// One test owns the process-global ring (this file is its own process).
#[test]
fn ring_layer_formats_events_and_the_ring_stays_consistent() {
    let _serial = RING_EDGES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let subscriber = tracing_subscriber::registry().with(RingLayer);
    let start = ring::recent(&LogQuery::default()).1;
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "ring_edges", "plain");
        tracing::warn!(target: "ring_edges", count = 3, name = "wolf", "with fields");
        tracing::error!(target: "ring_edges", count = 1);
        tracing::debug!(target: "ring_edges", "dbg {}", 7);
        tracing::trace!(target: "ring_edges", "trace");
    });
    let q = |f: &dyn Fn(&mut LogQuery)| {
        let mut query = LogQuery {
            since: Some(start),
            target: Some("ring_edges".into()),
            ..Default::default()
        };
        f(&mut query);
        ring::recent(&query).0
    };
    let all = q(&|_| {});
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].message, "t=0 plain");
    assert!(
        all[1].message.contains("with fields")
            && all[1].message.contains("count=3")
            && all[1].message.contains("name=wolf"),
        "{}",
        all[1].message
    );
    assert_eq!(
        all[2].message, "t=0 count=1",
        "a field-only event has one separator after the diagnostic tick"
    );
    assert_eq!(all[3].message, "t=0 dbg 7");
    assert_eq!(
        all.iter().map(|r| r.level.as_str()).collect::<Vec<_>>(),
        ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"]
    );
    assert!(
        all.windows(2).all(|w| w[1].seq == w[0].seq + 1),
        "consecutive sequence numbers"
    );
    assert!(all.iter().all(|r| r.unix_ms > 1_600_000_000_000));

    // Level filter: each minimum keeps that level and everything more severe.
    for (level, expected) in [
        (tracing::Level::ERROR, 1),
        (tracing::Level::WARN, 2),
        (tracing::Level::INFO, 3),
        (tracing::Level::DEBUG, 4),
        (tracing::Level::TRACE, 5),
    ] {
        assert_eq!(
            q(&|query| query.min_level = Some(level)).len(),
            expected,
            "{level}"
        );
    }
    // Limits, the empty `contains`, a `since` in the future, and a limit of zero.
    assert_eq!(q(&|query| query.limit = Some(0)).len(), 0);
    assert_eq!(q(&|query| query.limit = Some(100)).len(), 5);
    assert_eq!(
        q(&|query| query.limit = Some(2))
            .iter()
            .map(|r| r.message.as_str())
            .collect::<Vec<_>>(),
        ["t=0 dbg 7", "t=0 trace"]
    );
    assert_eq!(q(&|query| query.contains = Some(String::new())).len(), 5);
    assert_eq!(q(&|query| query.since = Some(u64::MAX)).len(), 0);
    assert_eq!(
        q(&|query| query.target = Some("ring_edges_nothing".into())).len(),
        0
    );
    let (_, next) = ring::recent(&LogQuery::default());
    assert_eq!(next, start + 5);

    // Wrapping past the capacity keeps the newest records with unbroken sequence numbers.
    for i in 0..CAPACITY + 3 {
        ring::push("INFO", "ring_edges_wrap", format!("n{i}"));
    }
    let (records, next) = ring::recent(&LogQuery::default());
    assert_eq!(records.len(), CAPACITY);
    assert_eq!(records.last().map(|r| r.seq + 1), Some(next));
    assert_eq!(
        records.last().map(|r| r.message.as_str()),
        Some(format!("n{}", CAPACITY + 2).as_str())
    );
    assert!(records.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    let mut seen = 0;
    assert!(ring::try_for_each_recent(3, |_| seen += 1));
    assert_eq!(seen, 3);
    let mut none = 0;
    assert!(ring::try_for_each_recent(0, |_| none += 1));
    assert_eq!(none, 0);
}

/// `recent` must clone at most `limit` records while it holds the ring lock, and still return
/// the newest matches, oldest first.
#[test]
fn recent_clones_only_the_limited_matches_under_the_lock() {
    let _serial = RING_EDGES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = ring::recent(&LogQuery::default()).1;
    let n = 1_000usize;
    let limit = 500usize;
    for i in 0..n {
        ring::push("INFO", "log_edges_limit_g", format!("m{i:04}"));
    }
    let (records, _) = ring::recent(&LogQuery {
        since: Some(start),
        target: Some("log_edges_limit_g".into()),
        limit: Some(limit),
        ..LogQuery::default()
    });
    assert_eq!(
        ring::recent_locked_clones(),
        limit as u64,
        "cloned every match under the lock, then dropped down to the limit"
    );
    assert_eq!(records.len(), limit);
    assert_eq!(records[0].message, format!("m{:04}", n - limit));
    assert_eq!(records[limit - 1].message, format!("m{:04}", n - 1));
    assert!(records.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[test]
fn recent_limit_keeps_the_newest_contains_matches() {
    let _serial = RING_EDGES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = ring::recent(&LogQuery::default()).1;
    for i in 0usize..20 {
        let message = if i.is_multiple_of(2) {
            format!("keep {i}")
        } else {
            format!("drop {i}")
        };
        ring::push("INFO", "log_edges_contains_g", message);
    }
    let records = ring::recent(&LogQuery {
        since: Some(start),
        target: Some("log_edges_contains_g".into()),
        contains: Some("keep".into()),
        limit: Some(3),
        ..LogQuery::default()
    })
    .0;
    let messages: Vec<&str> = records.iter().map(|r| r.message.as_str()).collect();
    assert_eq!(messages, ["keep 14", "keep 16", "keep 18"]);
}
