// SPDX-License-Identifier: MPL-2.0
//! Opt-in asset I/O diagnostics (ADR-8160).
//!
//! Set `REGIA_ASSET_TRACE` to an append-only log path before starting the process.
//! Unset or empty disables tracing. Configuration is sampled on first use, including
//! [`snapshot`]. Sink failures never replace an asset result. See
//! `book/agents/asset-access-trace.md` for coverage and the diagnostic text syntax.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::Serialize;

/// Maximum recent rows retained in memory; the file receives every row until a sink error.
pub const RECENT_CAPACITY: usize = 256;
/// Maximum UTF-8 bytes retained per diagnostic path in the in-memory view.
pub const RECENT_PATH_BYTES: usize = 4096;

/// One completed asset access, in process-local completion order.
#[derive(Clone, Debug, Serialize)]
pub struct AccessEvent {
    /// One-based sequence number within this process (saturating).
    pub sequence: u64,
    /// `ok`, `miss` (not found or not a directory), or `err` (any other failure).
    pub outcome: &'static str,
    /// `open` (loose read or pak open), `pak` (payload), or `resolve` (manifest lookup).
    pub operation: &'static str,
    /// Absolute loose-file path, pak name plus `#` and asset id, or logical manifest path.
    pub path: String,
    /// The recent view shortened the path; the log still receives its full display text.
    pub path_truncated: bool,
}

/// Read-only view of the process trace. Querying never reads the log file.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AccessTraceSnapshot {
    /// A nonempty `REGIA_ASSET_TRACE` was configured, even if opening it failed.
    pub enabled: bool,
    /// The sink is open and has not reported a write failure.
    pub sink_available: bool,
    /// `asset_trace_open` or `asset_trace_write`; `None` when no sink failure occurred.
    pub sink_error: Option<&'static str>,
    /// Number of completed traced operations (saturating).
    pub total: u64,
    /// Number of full rows successfully written to the sink (saturating).
    pub written: u64,
    /// Rows evicted from the bounded recent view (saturating).
    pub evicted: u64,
    /// Newest requested rows, in ascending sequence order; at most [`RECENT_CAPACITY`].
    pub events: Vec<AccessEvent>,
}

struct Trace {
    file: Option<File>,
    error: Option<&'static str>,
    total: u64,
    written: u64,
    recent: VecDeque<AccessEvent>,
}

fn trace() -> Option<&'static Mutex<Trace>> {
    static TRACE: OnceLock<Option<Mutex<Trace>>> = OnceLock::new();
    TRACE
        .get_or_init(|| {
            let path = std::env::var_os("REGIA_ASSET_TRACE").filter(|s| !s.is_empty())?;
            let (file, error) = match OpenOptions::new().create(true).append(true).open(path) {
                Ok(file) => (Some(file), None),
                Err(_) => (None, Some("asset_trace_open")),
            };
            Some(Mutex::new(Trace {
                file,
                error,
                total: 0,
                written: 0,
                recent: VecDeque::with_capacity(RECENT_CAPACITY),
            }))
        })
        .as_ref()
}

/// Returns up to `limit` recent events (clamped to [`RECENT_CAPACITY`]).
///
/// Disabled tracing returns an empty snapshot without opening a file. A zero limit
/// reports counters only. This call does not itself produce a trace row.
#[must_use]
pub fn snapshot(limit: usize) -> AccessTraceSnapshot {
    let Some(trace) = trace() else {
        return AccessTraceSnapshot::default();
    };
    let trace = trace.lock();
    AccessTraceSnapshot {
        enabled: true,
        sink_available: trace.file.is_some(),
        sink_error: trace.error,
        total: trace.total,
        written: trace.written,
        evicted: trace.total.saturating_sub(trace.recent.len() as u64),
        events: trace
            .recent
            .iter()
            .skip(trace.recent.len().saturating_sub(limit))
            .cloned()
            .collect(),
    }
}

fn record(operation: &'static str, outcome: &'static str, path: impl FnOnce() -> String) {
    let Some(trace) = trace() else { return };
    let _span = tracing::debug_span!("asset_access_trace", operation).entered();
    let mut path = path();
    let mut trace = trace.lock();
    trace.total = trace.total.saturating_add(1);
    // String Debug escapes line breaks, quotes and control characters. Build one row
    // before write_all; the mutex serializes threads, not independent processes.
    if let Some(file) = trace.file.as_mut() {
        let line = format!("{outcome} {operation} {path:?}\n");
        match file.write_all(line.as_bytes()) {
            Ok(()) => trace.written = trace.written.saturating_add(1),
            Err(_) => {
                trace.file = None;
                trace.error = Some("asset_trace_write");
            }
        }
    }
    let path_truncated = path.len() > RECENT_PATH_BYTES;
    if path_truncated {
        // Allocate the bounded prefix: truncate alone would retain an arbitrarily
        // large caller-supplied path allocation in every history slot.
        path = path[..path.floor_char_boundary(RECENT_PATH_BYTES)].to_owned();
    }
    let sequence = trace.total;
    if trace.recent.len() == RECENT_CAPACITY {
        trace.recent.pop_front();
    }
    trace.recent.push_back(AccessEvent {
        sequence,
        operation,
        outcome,
        path,
        path_truncated,
    });
}

fn io_outcome(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => "miss",
        _ => "err",
    }
}

pub(crate) fn note_io<T>(path: &Path, result: &io::Result<T>) {
    record("open", result.as_ref().err().map_or("ok", io_outcome), || {
        std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned()
    });
}

pub(crate) fn note_pak<T>(path: &str, id: crate::AssetId, result: &Result<T, crate::PakError>) {
    let outcome = match result {
        Ok(_) => "ok",
        Err(crate::PakError::NotFound(_)) => "miss",
        Err(crate::PakError::Io(error)) => io_outcome(error),
        Err(_) => "err",
    };
    record("pak", outcome, || format!("{path}#{id}"));
}

pub(crate) fn note_resolve(path: &str, found: bool) {
    record("resolve", if found { "ok" } else { "miss" }, || path.to_owned());
}
