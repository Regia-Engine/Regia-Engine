// SPDX-License-Identifier: MPL-2.0
//! Per-asset load times (P-314, `book/agents/profiler.md` "Loads"): every load the
//! [`crate::AssetServer`] runs records how long it read the cooked bytes, decoded them and ran the
//! type's prepare (upload) step, with the bytes read and where they came from. The editor Profiler's
//! Loads tab and `editor.profiler_loads` list them; [`crate::AssetServer::load_times`] reads them.
//!
//! The table keeps one row per asset (its newest load; a reload replaces it) and at most
//! [`MAX_LOAD_TIMES`] rows: past that the oldest row is dropped and counted. Typed loads
//! (`load_mesh`, `load_texture`, ...) have no prepare step here (their GPU upload is the renderer's),
//! so their `upload_ns` is 0; counted loads (`load::<T>`) time their [`crate::PrepareAsset`] step.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;

use crate::id::AssetId;

/// Most rows the table keeps.
pub const MAX_LOAD_TIMES: usize = 4096;

/// One asset's newest load.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LoadTime {
    /// The asset.
    pub id: AssetId,
    /// The load's kind (`mesh`, `texture`, `image`, `material`, `model`, or a loader's kind).
    pub kind: &'static str,
    /// Cooked bytes read (the whole cooked file or pak entry).
    pub bytes: u64,
    /// Time reading the bytes, ns.
    pub read_ns: u64,
    /// Time checking and decoding them, ns.
    pub decode_ns: u64,
    /// Time in the type's prepare (upload) step, ns (0 when it has none).
    pub upload_ns: u64,
    /// `pak` or `cache` (empty when the read failed).
    pub source: &'static str,
    /// Whether the load succeeded.
    pub ok: bool,
    /// Loads recorded before this one by the server.
    pub seq: u64,
}

impl LoadTime {
    /// Read, decode and upload together, ns.
    #[must_use]
    pub fn total_ns(&self) -> u64 {
        self.read_ns
            .saturating_add(self.decode_ns)
            .saturating_add(self.upload_ns)
    }
}

/// The server's load-time table.
#[derive(Debug, Default)]
pub struct LoadTimes {
    rows: HashMap<AssetId, LoadTime>,
    next: u64,
    dropped: u64,
}

impl LoadTimes {
    /// Records a load (replacing the asset's earlier row).
    pub fn record(&mut self, mut row: LoadTime) {
        row.seq = self.next;
        self.next += 1;
        if !self.rows.contains_key(&row.id)
            && self.rows.len() >= MAX_LOAD_TIMES
            && let Some(oldest) = self.rows.values().min_by_key(|r| r.seq).map(|r| r.id)
        {
            self.rows.remove(&oldest);
            self.dropped += 1;
        }
        self.rows.insert(row.id, row);
    }

    /// Every kept row, oldest load first.
    #[must_use]
    pub fn rows(&self) -> Vec<LoadTime> {
        let mut rows: Vec<LoadTime> = self.rows.values().cloned().collect();
        rows.sort_by_key(|r| r.seq);
        rows
    }

    /// The newest load of `id`.
    #[must_use]
    pub fn get(&self, id: AssetId) -> Option<&LoadTime> {
        self.rows.get(&id)
    }

    /// Loads recorded since the server opened.
    #[must_use]
    pub fn recorded(&self) -> u64 {
        self.next
    }

    /// Rows dropped at [`MAX_LOAD_TIMES`].
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

thread_local! {
    /// The read time and bytes of the load in progress on this thread.
    static READ: Cell<(u64, u64, &'static str)> = const { Cell::new((0, 0, "")) };
}

/// Starts timing a load on this thread: clears the read accumulator and returns the start.
pub(crate) fn begin() -> Instant {
    READ.with(|r| r.set((0, 0, "")));
    Instant::now()
}

/// Ns from `start` to now.
pub(crate) fn since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Adds a read of `bytes` from `source` that took `ns` to the load in progress on this thread.
pub(crate) fn note_read(ns: u64, bytes: u64, source: &'static str) {
    READ.with(|r| {
        let (t, b, _) = r.get();
        r.set((t.saturating_add(ns), b.saturating_add(bytes), source));
    });
}

/// Times `read` as a read of the load in progress on this thread (bytes: the returned length).
pub(crate) fn timed_read<T: AsRef<[u8]>, E>(
    source: &'static str,
    read: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let start = Instant::now();
    let out = read();
    let bytes = out.as_ref().map_or(0, |b| b.as_ref().len() as u64);
    note_read(since(start), bytes, source);
    out
}

/// The row of a finished typed load that started at `start` on this thread: the reads noted since
/// [`begin`] are its read time, the rest its decode time.
pub(crate) fn finish(start: Instant, id: AssetId, kind: &'static str, ok: bool) -> LoadTime {
    let total = since(start);
    let (read_ns, bytes, source) = READ.with(Cell::get);
    LoadTime {
        id,
        kind,
        bytes,
        read_ns,
        decode_ns: total.saturating_sub(read_ns),
        upload_ns: 0,
        source,
        ok,
        seq: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: AssetId) -> LoadTime {
        LoadTime {
            id,
            kind: "mesh",
            bytes: 10,
            read_ns: 1,
            decode_ns: 2,
            upload_ns: 3,
            source: "cache",
            ok: true,
            seq: 0,
        }
    }

    #[test]
    fn a_reload_replaces_the_row_and_rows_list_in_load_order() {
        let (a, b) = (AssetId::new_random(), AssetId::new_random());
        let mut table = LoadTimes::default();
        table.record(row(a));
        table.record(row(b));
        table.record(LoadTime { bytes: 20, ..row(a) });
        let rows = table.rows();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [b, a]);
        assert_eq!(rows[1].bytes, 20);
        assert_eq!((table.recorded(), table.dropped()), (3, 0));
        assert_eq!(rows[0].total_ns(), 6);
    }

    #[test]
    fn the_table_drops_its_oldest_row_when_full() {
        let mut table = LoadTimes::default();
        let first = AssetId::new_random();
        table.record(row(first));
        for _ in 0..MAX_LOAD_TIMES {
            table.record(row(AssetId::new_random()));
        }
        assert_eq!(table.rows().len(), MAX_LOAD_TIMES);
        assert_eq!(table.dropped(), 1);
        assert!(table.get(first).is_none(), "the oldest went");
    }

    #[test]
    fn a_finished_load_splits_read_from_decode() {
        let start = begin();
        let read: Result<Vec<u8>, ()> = timed_read("pak", || Ok(vec![0u8; 64]));
        assert_eq!(read.map(|b| b.len()), Ok(64));
        let row = finish(start, AssetId::new_random(), "texture", true);
        assert_eq!((row.bytes, row.source, row.ok), (64, "pak", true));
        assert!(row.read_ns <= row.read_ns + row.decode_ns);
        assert_eq!(row.upload_ns, 0);
    }
}
