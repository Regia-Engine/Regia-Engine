// SPDX-License-Identifier: MPL-2.0
//! In-memory ring of recent log records, for the agent protocol's `read_logs` and the editor's output
//! log. Always on; bounded ([`CAPACITY`] records); each record has a monotonically increasing sequence
//! number so readers can poll with `since`.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Records kept in memory.
pub const CAPACITY: usize = 4096;

/// One captured log record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogRecord {
    /// Sequence number, increasing by one per record since process start.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub unix_ms: u64,
    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub level: String,
    /// Target (usually the crate or module).
    pub target: String,
    /// Formatted message and fields.
    pub message: String,
}

struct Ring {
    next_seq: u64,
    records: VecDeque<Arc<LogRecord>>,
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| {
        Mutex::new(Ring {
            next_seq: 0,
            records: VecDeque::with_capacity(CAPACITY),
        })
    })
}

/// Filter for [`recent`].
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Only records with `seq >= since`.
    pub since: Option<u64>,
    /// Minimum level (`error` < `warn` < `info` < `debug` < `trace`); `None` = all.
    pub min_level: Option<tracing::Level>,
    /// Only targets starting with this prefix.
    pub target: Option<String>,
    /// Only messages containing this text.
    pub contains: Option<String>,
    /// At most this many records (the newest ones); `None` = all matching.
    pub limit: Option<usize>,
}

fn level_rank(level: &str) -> u8 {
    match level {
        "ERROR" => 1,
        "WARN" => 2,
        "INFO" => 3,
        "DEBUG" => 4,
        _ => 5,
    }
}

thread_local! {
    /// [`recent`]'s `Arc` clones while the ring lock was held, on this thread.
    static RECENT_LOCKED_CLONES: Cell<u64> = const { Cell::new(0) };
}

/// How many records the last [`recent`] on this thread cloned while it held the ring lock.
///
/// Not part of the host API. A query with `limit` clones at most that many.
#[doc(hidden)]
#[must_use]
pub fn recent_locked_clones() -> u64 {
    RECENT_LOCKED_CLONES.with(Cell::get)
}

/// Matching records, oldest first, plus the sequence number the next record will get.
pub fn recent(query: &LogQuery) -> (Vec<LogRecord>, u64) {
    let max_rank = query.min_level.map(|l| level_rank(l.as_str())).unwrap_or(u8::MAX);
    // A target named with an engine crate's name from before the rename also matches the
    // renamed crate's records (ADR-1429).
    let current_target = query.target.as_deref().and_then(crate::brand::current_log_target);
    let target_matches = |target: &str| {
        query.target.as_deref().is_none_or(|t| {
            target.starts_with(t) || current_target.as_deref().is_some_and(|c| target.starts_with(c))
        })
    };
    // Walk newest-first and stop after `limit` matches, so a panel that asks for 500 records
    // does not clone the rest of the ring while `push` waits. The string copies happen after
    // the lock is released. Sequence numbers increase as records are appended, so the first
    // record older than `since` ends the walk.
    let limit = query.limit.unwrap_or(usize::MAX);
    let mut picked: Vec<Arc<LogRecord>> = Vec::new();
    let next_seq = {
        let ring = ring().lock();
        if limit > 0 {
            for record in ring.records.iter().rev() {
                if query.since.is_some_and(|since| record.seq < since) {
                    break;
                }
                if level_rank(&record.level) > max_rank || !target_matches(&record.target) {
                    continue;
                }
                if query
                    .contains
                    .as_deref()
                    .is_some_and(|text| !record.message.contains(text))
                {
                    continue;
                }
                picked.push(Arc::clone(record));
                if picked.len() == limit {
                    break;
                }
            }
        }
        RECENT_LOCKED_CLONES.with(|clones| clones.set(picked.len() as u64));
        ring.next_seq
    };
    picked.reverse();
    (picked.iter().map(|r| LogRecord::clone(r)).collect(), next_seq)
}

/// Visits the newest `limit` records, oldest first, **without allocating or blocking**: returns
/// `false` without calling `f` when the ring is locked (for example by the thread that crashed)
/// or was never created (nothing was logged yet; creating it would allocate).
/// For the native crash handler, which runs in a signal handler.
pub fn try_for_each_recent(limit: usize, mut f: impl FnMut(&LogRecord)) -> bool {
    let Some(ring) = RING.get().and_then(Mutex::try_lock) else {
        return false;
    };
    let skip = ring.records.len().saturating_sub(limit);
    for r in ring.records.iter().skip(skip) {
        f(r.as_ref());
    }
    true
}

/// Appends a record (used by the layer; public for tests and non-`tracing` sources).
pub fn push(level: &str, target: &str, message: String) {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut ring = ring().lock();
    let seq = ring.next_seq;
    ring.next_seq += 1;
    if ring.records.len() == CAPACITY {
        ring.records.pop_front();
    }
    ring.records.push_back(Arc::new(LogRecord {
        seq,
        unix_ms,
        level: level.into(),
        target: target.into(),
        message,
    }));
}

#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else if !field.name().starts_with("log.") {
            let sep = if self.0.is_empty() { "" } else { " " };
            let _ = write!(self.0, "{sep}{}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else if field.name() == "log.target" {
            // handled by the caller
        } else if !field.name().starts_with("log.") {
            let sep = if self.0.is_empty() { "" } else { " " };
            let _ = write!(self.0, "{sep}{}={value}", field.name());
        }
    }
}

/// Visitor that only extracts `log.target` (records bridged from the `log` crate carry their real
/// target there; the event's own target is just "log").
#[derive(Default)]
struct LogTarget(Option<String>);

impl Visit for LogTarget {
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "log.target" {
            self.0 = Some(value.to_string());
        }
    }
}

/// `tracing` layer feeding the ring.
pub struct RingLayer;

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        let mut log_target = LogTarget::default();
        event.record(&mut log_target);
        let meta = event.metadata();
        let target = log_target.0.unwrap_or_else(|| meta.target().to_string());
        push(
            meta.level().as_str(),
            &target,
            format!("t={} {}", super::tick(), message.0),
        );
    }
}

/// Serializes unit tests that write the process-global ring, including peer-session tests.
#[cfg(test)]
pub(crate) static RING_TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_for_each_recent_visits_newest_and_reports_a_held_lock() {
        let _serial = RING_TESTS.lock();
        push("INFO", "ring_test::signal", "one".into());
        push("INFO", "ring_test::signal", "two".into());
        let mut seen = Vec::new();
        assert!(try_for_each_recent(2, |r| seen.push(r.message.clone())));
        assert_eq!(seen, ["one", "two"]);
        let held = ring().lock();
        assert!(!try_for_each_recent(2, |_| panic!("visited a locked ring")));
        drop(held);
    }

    #[test]
    fn try_for_each_recent_visits_without_creating_and_yields_to_a_lock() {
        let _serial = RING_TESTS.lock();
        push("INFO", "ring_test::try", "visited".into());
        let mut seen = 0;
        assert!(try_for_each_recent(usize::MAX, |r| {
            if r.target == "ring_test::try" {
                seen += 1;
            }
        }));
        assert!(seen >= 1);
        // The ring exists now (`push` made it); a held lock makes the crash-path reader give up.
        let held = RING.get().map(Mutex::lock);
        assert!(held.is_some());
        assert!(!try_for_each_recent(1, |_| panic!(
            "must not visit a locked ring"
        )));
    }

    #[test]
    fn query_filters_and_limits() {
        let _serial = RING_TESTS.lock();
        let start = recent(&LogQuery::default()).1;
        push("INFO", "ring_test::a", "hello world".into());
        push("WARN", "ring_test::b", "careful".into());
        push("DEBUG", "ring_test::a", "details".into());
        let since = Some(start);
        let all = recent(&LogQuery {
            since,
            target: Some("ring_test".into()),
            ..Default::default()
        })
        .0;
        assert_eq!(all.len(), 3);
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
        let warn_up = LogQuery {
            since,
            target: Some("ring_test".into()),
            min_level: Some(tracing::Level::WARN),
            ..Default::default()
        };
        assert_eq!(
            recent(&warn_up)
                .0
                .iter()
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>(),
            ["careful"]
        );
        let contains = LogQuery {
            since,
            contains: Some("world".into()),
            ..Default::default()
        };
        assert_eq!(recent(&contains).0.len(), 1);
        let last = LogQuery {
            since,
            target: Some("ring_test".into()),
            limit: Some(1),
            ..Default::default()
        };
        assert_eq!(recent(&last).0[0].message, "details");
    }

    #[test]
    fn a_legacy_crate_target_matches_the_renamed_crate() {
        let _serial = RING_TESTS.lock();
        let start = recent(&LogQuery::default()).1;
        let current = format!("{}agent::ring_test", crate::brand::CRATE_PREFIX);
        push("INFO", &current, "from the renamed crate".into());
        let legacy = LogQuery {
            since: Some(start),
            target: Some(format!("{}agent", crate::brand::LEGACY_CRATE_PREFIX)),
            ..Default::default()
        };
        assert_eq!(
            recent(&legacy)
                .0
                .iter()
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>(),
            ["from the renamed crate"]
        );
    }

    #[test]
    fn ring_is_bounded() {
        let _serial = RING_TESTS.lock();
        for i in 0..CAPACITY + 10 {
            push("TRACE", "ring_test::bound", format!("{i}"));
        }
        let (records, next) = recent(&LogQuery::default());
        assert!(records.len() <= CAPACITY);
        assert_eq!(records.last().map(|r| r.seq + 1), Some(next));
    }
}
